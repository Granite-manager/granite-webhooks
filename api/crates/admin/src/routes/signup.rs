//! Self sign-up (ENG-90): a company Granite Manager does not know creates
//! its own account from the landing page.
//!
//! - `GET  /v1/public/signup`                      → the monthly price and who to contact
//! - `GET  /v1/public/signup/subdomain/{subdomain}` → is a web address free
//! - `POST /v1/public/signup`                       → create the company and its manager, sign in
//!
//! No session. The new company is seeded like one a superuser creates
//! (`superuser_companies::seed_company`) and pays a flat
//! `SIGNUP_MONTHLY_PRICE_CENTS` a month (`first_payment`). Until Stripe
//! confirms the first payment, or a superuser gives it free time
//! (`company.free_access_until`, after it reached out for a demo), the API
//! keeps it out (`auth::paywall`).
//! The web address it asks for is kept as `requested_subdomain` until a
//! superuser has DNS and Caddy serve it. Granite Manager's support inbox
//! hears of every sign-up.
//!
//! Abuse: a filled honeypot field is refused, an address may sign up at
//! most `MAX_SIGNUPS_PER_ADDRESS` companies a day, and an email that already
//! signs in somewhere is refused (sign in instead).

use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::auth::{client_ip, start_session};
use crate::routes::superuser_companies::{seed_company, subdomain_taken, valid_subdomain};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use common::utils::email_body::escape_html;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(signup_options, sign_up))
        .routes(routes!(check_subdomain))
}

/// Companies one client address may sign up in a day.
const MAX_SIGNUPS_PER_ADDRESS: i64 = 3;
/// Shortest password (as everywhere else).
const MIN_PASSWORD: usize = 8;
/// bcrypt cost of new passwords (as everywhere else).
const BCRYPT_COST: u32 = 10;
/// Where Granite Manager hears of new sign-ups.
const SIGNUP_NOTICE_TO: &str = "support@granite-manager.com";
/// Who a new company writes to for a free month and a demo.
pub const SALES_EMAIL: &str = "sales@granite-manager.com";
/// What a company from the sign-up page pays a month, in USD cents ($800).
pub const SIGNUP_MONTHLY_PRICE_CENTS: i32 = 80_000;
/// `positions.id` of a manager.
const MANAGER: i32 = 5;
/// Web addresses no company may have.
const RESERVED_SUBDOMAINS: [&str; 20] = [
    "www", "api", "app", "admin", "mail", "email", "smtp", "imap", "ftp", "dev", "staging", "test",
    "demo", "support", "help", "billing", "status", "static", "cdn", "granite",
];

/// `GET /v1/public/signup`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignupOptions {
    /// What the company pays a month, in USD cents (80000: $800).
    pub monthly_price_cents: i32,
    /// Who to write to for a free month and a demo instead of paying now.
    pub contact_email: String,
    /// The base domain of company web addresses (`granite-manager.com`).
    pub base_domain: String,
}

/// What signing up costs
///
/// The flat monthly price (USD cents), the address to write to for a free
/// month and a demo instead, and the domain web addresses go under. Then
/// check a web address with `GET /v1/public/signup/subdomain/{subdomain}`
/// and sign up with `POST /v1/public/signup`. No session.
#[crate::openapi::path(
    get,
    path = "/v1/public/signup",
    operation_id = "get_signup_options",
    tag = "public",
    responses(
        (status = 200, description = "The price and the contact", body = SignupOptions),
    )
)]
pub async fn signup_options(State(state): State<AppState>) -> Json<SignupOptions> {
    Json(SignupOptions {
        monthly_price_cents: SIGNUP_MONTHLY_PRICE_CENTS,
        contact_email: SALES_EMAIL.into(),
        base_domain: crate::setup_checklist::base_domain(&state),
    })
}

/// Why a web address cannot be had, or None when it is fine to ask for.
fn subdomain_problem(subdomain: &str) -> Option<&'static str> {
    if subdomain.len() < 3 || subdomain.len() > 40 {
        return Some("The web address must be 3 to 40 characters");
    }
    if !valid_subdomain(subdomain) {
        return Some("Use only lowercase letters, numbers and single hyphens");
    }
    if RESERVED_SUBDOMAINS.contains(&subdomain) {
        return Some("This web address is reserved");
    }
    None
}

/// `GET /v1/public/signup/subdomain/{subdomain}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SubdomainCheck {
    /// The address, lowercased.
    pub subdomain: String,
    /// It can be signed up with.
    pub available: bool,
    /// Why not, for the form; null when available.
    pub reason: Option<String>,
}

/// Check a web address
///
/// Whether `subdomain` (`acme` of `acme.granite-manager.com`) is well formed
/// (3 to 40 lowercase letters, numbers and single hyphens), not reserved and
/// not taken or asked for by another company. No session.
#[crate::openapi::path(
    get,
    path = "/v1/public/signup/subdomain/{subdomain}",
    operation_id = "check_signup_subdomain",
    tag = "public",
    params(("subdomain" = String, Path, description = "The web address to check, e.g. `acme`")),
    responses(
        (status = 200, description = "Whether it is available", body = SubdomainCheck),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn check_subdomain(
    State(state): State<AppState>,
    Path(subdomain): Path<String>,
) -> ApiResult<Json<SubdomainCheck>> {
    let subdomain = subdomain.trim().to_lowercase();
    let reason = match subdomain_problem(&subdomain) {
        Some(problem) => Some(problem.to_string()),
        None if subdomain_taken(&state, &subdomain, 0).await? => {
            Some("This web address is taken".to_string())
        }
        None => None,
    };
    Ok(Json(SubdomainCheck {
        available: reason.is_none(),
        reason,
        subdomain,
    }))
}

/// Body of `POST /v1/public/signup`.
#[derive(Deserialize, ToSchema)]
pub struct SignupInput {
    /// The company's name, 1 to 120 characters.
    pub company_name: String,
    /// The web address it wants (`acme` of `acme.granite-manager.com`); see
    /// `GET /v1/public/signup/subdomain/{subdomain}`.
    pub subdomain: String,
    /// The company's address; optional.
    #[serde(default)]
    pub address: Option<String>,
    /// The person signing up: their name, 1 to 100 characters. They become
    /// the company's Manager.
    pub name: String,
    /// Their email, which they sign in with. Must not sign in anywhere yet.
    pub email: String,
    /// Their phone number, at least 10 digits.
    pub phone_number: String,
    /// Their password, 8 to 200 characters.
    pub password: String,
    /// They accept the End-User License Agreement and the Privacy Policy.
    pub accept_terms: bool,
    /// Leave empty (a field people do not see; bots fill it in).
    #[serde(default)]
    pub website: Option<String>,
}

impl std::fmt::Debug for SignupInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupInput")
            .field("company_name", &self.company_name)
            .field("subdomain", &self.subdomain)
            .field("email", &self.email)
            .finish_non_exhaustive()
    }
}

/// The sign-up as it is saved.
#[derive(Debug, PartialEq, Eq)]
struct CleanSignup {
    company_name: String,
    subdomain: String,
    address: Option<String>,
    name: String,
    email: String,
    phone: String,
    password: String,
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn plausible_email(email: &str) -> bool {
    let Some((local, host)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !email.contains(char::is_whitespace)
        && email.len() <= 254
}

fn clean(input: &SignupInput) -> ApiResult<CleanSignup> {
    if input
        .website
        .as_deref()
        .is_some_and(|w| !w.trim().is_empty())
    {
        return Err(bad("Please try again."));
    }
    let company_name = input.company_name.trim().to_string();
    if company_name.is_empty() || company_name.chars().count() > 120 {
        return Err(bad("Enter your company's name (up to 120 characters)"));
    }
    let subdomain = input.subdomain.trim().to_lowercase();
    if let Some(problem) = subdomain_problem(&subdomain) {
        return Err(bad(problem));
    }
    let address = input
        .address
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    if address.as_ref().is_some_and(|a| a.chars().count() > 255) {
        return Err(bad("The address is too long"));
    }
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad("Enter your name (up to 100 characters)"));
    }
    let email = input.email.trim().to_lowercase();
    if !plausible_email(&email) {
        return Err(bad("Enter a valid email"));
    }
    let phone = input.phone_number.trim().to_string();
    if phone.chars().filter(char::is_ascii_digit).count() < 10 || phone.len() > 32 {
        return Err(bad("Enter a phone number with at least 10 digits"));
    }
    let password = input.password.trim().to_string();
    if password.chars().count() < MIN_PASSWORD || password.len() > 200 {
        return Err(bad("The password must be 8 to 200 characters"));
    }
    if !input.accept_terms {
        return Err(bad(
            "Please accept the End-User License Agreement and the Privacy Policy",
        ));
    }
    Ok(CleanSignup {
        company_name,
        subdomain,
        address,
        name,
        email,
        phone,
        password,
    })
}

/// Sign up a new company
///
/// Creates the company (seeded like one a superuser creates: calendars,
/// starter pipeline, email templates, a walk-in kiosk login), makes the
/// person signing up its Manager and an admin, sets its price ($800 a
/// month) and signs them in: the answer and the session cookie are those
/// of `POST /v1/auth/login`. Everything they typed is saved here, before
/// any payment. Next they pay (`POST /v1/billing/first-payment/checkout`,
/// to Stripe) or write to Granite Manager for a free month and a demo.
/// Until Stripe shows the payment, or a superuser gives free time, the API
/// refuses everything but sign-in and billing (`company.source =
/// 'signup_page'`, `auth::paywall`). The web address is kept as asked for;
/// Granite Manager turns it on later, so the company starts on the main
/// host. No session.
///
/// 400 for a missing or bad field, 409 when the email already signs in
/// somewhere (sign in instead) or the web address is taken, 429 after
/// `MAX_SIGNUPS_PER_ADDRESS` sign-ups from one address in a day.
#[crate::openapi::path(
    post,
    path = "/v1/public/signup",
    operation_id = "sign_up_company",
    tag = "public",
    request_body = SignupInput,
    responses(
        (status = 200, description = "Created and signed in; the session cookie is set", body = crate::routes::auth::AuthSession),
        (status = 400, description = "A field is missing or bad", body = ErrorBody),
        (status = 409, description = "The email already has an account, or the web address is taken", body = ErrorBody),
        (status = 429, description = "Too many sign-ups from this address today", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn sign_up(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(input): JsonBody<SignupInput>,
) -> ApiResult<Response> {
    let signup = clean(&input)?;
    let pool = &state.pool;
    let ip_hash = crate::auth::sha256_hex(&client_ip(&headers));
    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM company
          WHERE signup_ip_hash = ? AND signed_up_at > UTC_TIMESTAMP() - INTERVAL 1 DAY",
    )
    .bind(&ip_hash)
    .fetch_one(pool)
    .await?;
    if recent >= MAX_SIGNUPS_PER_ADDRESS {
        return Err(ApiError::TooManyRequests(
            "Too many sign-ups from this network today. Please contact Granite Manager.".into(),
        ));
    }
    let email_used: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 LIMIT 1",
    )
    .bind(&signup.email)
    .fetch_optional(pool)
    .await?;
    if email_used.is_some() {
        return Err(ApiError::Conflict(
            "An account with this email already exists. Sign in instead.".into(),
        ));
    }
    if subdomain_taken(&state, &signup.subdomain, 0).await? {
        return Err(ApiError::Conflict("This web address is taken".into()));
    }
    let password = signup.password.clone();
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let now = crate::billing::now();

    let mut tx = pool.begin().await?;
    let company_id = sqlx::query(
        "INSERT INTO company (name, address, requested_subdomain, statistics_widgets_hidden,
                              billing_required, source, signed_up_at, signup_ip_hash,
                              subscription_price_cents)
         VALUES (?, ?, ?, 1, 1, ?, ?, ?, ?)",
    )
    .bind(&signup.company_name)
    .bind(&signup.address)
    .bind(&signup.subdomain)
    .bind(crate::auth::paywall::SIGNUP_PAGE)
    .bind(now)
    .bind(&ip_hash)
    .bind(SIGNUP_MONTHLY_PRICE_CENTS)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let company_id = i32::try_from(company_id)
        .map_err(|_| ApiError::Internal("company id out of range".into()))?;
    seed_company(&mut tx, company_id, &signup.subdomain).await?;
    let user_id = sqlx::query(
        "INSERT INTO users (name, email, phone_number, password, company_id, is_employee, is_admin)
         VALUES (?, ?, ?, ?, ?, 1, 1)",
    )
    .bind(&signup.name)
    .bind(&signup.email)
    .bind(&signup.phone)
    .bind(&hash)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let user_id =
        i32::try_from(user_id).map_err(|_| ApiError::Internal("user id out of range".into()))?;
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(MANAGER)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // The sales tax from the address it gave; one that can't be placed
    // leaves none (it can be fixed on the Company page).
    let tax = crate::company_tax::lookup_address(&state, signup.address.as_deref()).await;
    if let Some(tax) = tax.filter(|found| *found != crate::company_tax::TaxLookup::NotFound) {
        crate::company_tax::apply(&state.pool, company_id, Some(&tax)).await?;
    }

    let base = crate::setup_checklist::base_domain(&state);
    send_notices(&state, company_id, &signup, &base).await;
    tracing::info!(company_id, user_id, "company signed itself up");
    start_session(&state, user_id).await
}

/// Tells Granite Manager about the sign-up and welcomes the new manager.
/// Failures are logged, never shown: the account exists either way.
async fn send_notices(state: &AppState, company_id: i32, signup: &CleanSignup, base_domain: &str) {
    let price = format!("${} a month", SIGNUP_MONTHLY_PRICE_CENTS / 100);
    let notice = email(
        SIGNUP_NOTICE_TO,
        format!("New sign-up: {}", signup.company_name),
        format!(
            r#"<div style="font-family: Arial, sans-serif;">
  <h2>{company} signed up</h2>
  <p>Company id {company_id}. Manager: {name} &lt;{email}&gt;, {phone}.</p>
  <p>Web address asked for: <b>{subdomain}.{base}</b> (set the subdomain once DNS and Caddy serve it).</p>
  <p>Address: {address}</p>
  <p>Price: {price}. They are asked to pay with Stripe now, or to write to {sales} for a free month and a demo. Their account stays locked until they pay or you give them free time (Superuser &gt; Companies &gt; Free month).</p>
  <p>Still to set up for them: the web address, a phone line (Telnyx), the email domain, the website lead form and the stones on their website.</p>
</div>"#,
            company = escape_html(&signup.company_name),
            name = escape_html(&signup.name),
            email = escape_html(&signup.email),
            phone = escape_html(&signup.phone),
            sales = SALES_EMAIL,
            subdomain = escape_html(&signup.subdomain),
            base = escape_html(base_domain),
            address = escape_html(signup.address.as_deref().unwrap_or("not given")),
            price = escape_html(&price),
        ),
    );
    let app = state.app_url.trim_end_matches('/');
    let welcome = email(
        &signup.email,
        "Welcome to Granite Manager".into(),
        format!(
            r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Welcome, {name}</h2>
  <p style="color: #475569; line-height: 1.6;">{company} is set up on Granite Manager ({price}). If you did not pay yet, sign in and pay on the Billing page to open your account, or write to {sales} for a free month and a demo.</p>
  <p style="color: #475569; line-height: 1.6;">Your dashboard lists what is left to set up. For a phone line, your company email and your website, our team will help: reply to this email or write to {support}.</p>
  <a href="{app}/login" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Open Granite Manager</a>
</div>"#,
            name = escape_html(&signup.name),
            company = escape_html(&signup.company_name),
            price = escape_html(&price),
            sales = SALES_EMAIL,
            support = SIGNUP_NOTICE_TO,
        ),
    );
    let (sent_notice, sent_welcome) = tokio::join!(
        state.webhooks.send_email(&notice),
        state.webhooks.send_email(&welcome)
    );
    if let Err(err) = sent_notice {
        tracing::warn!(?err, company_id, "sign-up notice email failed");
    }
    if let Err(err) = sent_welcome {
        tracing::warn!(?err, company_id, "sign-up welcome email failed");
    }
}

fn email(to: &str, subject: String, body: String) -> OutgoingEmail {
    OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: "noreply@granite-manager.com".into(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        body,
        signature: None,
        reply_to: vec![SIGNUP_NOTICE_TO.to_string()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> SignupInput {
        SignupInput {
            company_name: "  Acme Stone ".into(),
            subdomain: " Acme-Stone ".into(),
            address: Some(" ".into()),
            name: " Ann Lee ".into(),
            email: " Ann@Acme.COM ".into(),
            phone_number: " (317) 555-0100 ".into(),
            password: " secret-pass ".into(),
            accept_terms: true,
            website: None,
        }
    }

    #[test]
    fn a_sign_up_is_trimmed_and_lowercased() {
        assert_eq!(
            clean(&input()).unwrap(),
            CleanSignup {
                company_name: "Acme Stone".into(),
                subdomain: "acme-stone".into(),
                address: None,
                name: "Ann Lee".into(),
                email: "ann@acme.com".into(),
                phone: "(317) 555-0100".into(),
                password: "secret-pass".into(),
            }
        );
    }

    #[test]
    fn bad_sign_ups_are_refused() {
        let refused = |change: fn(&mut SignupInput)| {
            let mut input = input();
            change(&mut input);
            clean(&input).is_err()
        };
        assert!(refused(|i| i.company_name = " ".into()));
        assert!(refused(|i| i.subdomain = "ab".into()));
        assert!(refused(|i| i.subdomain = "acme--stone".into()));
        assert!(refused(|i| i.subdomain = "acme.stone".into()));
        assert!(refused(|i| i.subdomain = "www".into()));
        assert!(refused(|i| i.name = String::new()));
        assert!(refused(|i| i.email = "ann@acme".into()));
        assert!(refused(|i| i.email = "ann acme.com".into()));
        assert!(refused(|i| i.phone_number = "555-0100".into()));
        assert!(refused(|i| i.password = "short".into()));
        assert!(refused(|i| i.accept_terms = false));
        assert!(refused(|i| i.website = Some("http://spam".into())));
    }
}
