//! Self sign-up (ENG-90): a company Granite Manager does not know creates
//! its own account from the landing page.
//!
//! - `GET  /v1/public/signup`                      → the features to pick and their prices
//! - `GET  /v1/public/signup/subdomain/{subdomain}` → is a web address free
//! - `POST /v1/public/signup`                       → create the company and its manager, sign in
//!
//! No session. The new company is seeded like one a superuser creates
//! (`superuser_companies::seed_company`) and pays for the features it picks
//! right away (`billing_features`: their prices added up, 20% off for every
//! feature). There is no free month: until Stripe confirms the first
//! payment the billing gate keeps it on `/admin/billing`.
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
use crate::routes::billing_features::{
    ALL_FEATURES_DISCOUNT_PERCENT, FeatureQuote, PricedFeature, priced_features, quote,
    save_choices,
};
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
    /// Sign-up is open: some feature has a price. False: the page says to
    /// contact Granite Manager.
    pub open: bool,
    /// The features to pick from, each with its monthly price, by name.
    pub features: Vec<PricedFeature>,
    /// Percent off when every feature is picked (20).
    pub all_features_discount_percent: i64,
    /// The base domain of company web addresses (`granite-manager.com`).
    pub base_domain: String,
}

/// What signing up offers
///
/// The features a new company can pay for, each with its monthly price
/// (USD cents); a company pays the sum of those it picks, 20% off when it
/// picks all of them, from the day it signs up. Then check a web address
/// with `GET /v1/public/signup/subdomain/{subdomain}` and sign up with
/// `POST /v1/public/signup`. No session.
#[crate::openapi::path(
    get,
    path = "/v1/public/signup",
    operation_id = "get_signup_options",
    tag = "public",
    responses(
        (status = 200, description = "The features and their prices", body = SignupOptions),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn signup_options(State(state): State<AppState>) -> ApiResult<Json<SignupOptions>> {
    let features = priced_features(&state.pool).await?;
    Ok(Json(SignupOptions {
        open: !features.is_empty(),
        features,
        all_features_discount_percent: ALL_FEATURES_DISCOUNT_PERCENT,
        base_domain: crate::setup_checklist::base_domain(&state),
    }))
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
    /// Their password, 8 to 200 characters.
    pub password: String,
    /// Ids of the features (`GET /v1/public/signup`) the company pays for; at
    /// least one. Unknown and unpriced ids are left out.
    #[serde(default)]
    pub feature_ids: Vec<i32>,
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
        password,
    })
}

/// Sign up a new company
///
/// Creates the company (seeded like one a superuser creates: calendars,
/// starter pipeline, email templates, a walk-in kiosk login), makes the
/// person signing up its Manager and an admin, saves the features it pays
/// for and their price, and signs them in: the answer and the session
/// cookie are those of `POST /v1/auth/login`. Next call
/// `POST /v1/billing/features/checkout` and send them to Stripe: there is
/// no free month, and until the first payment the billing gate keeps the
/// company on `/admin/billing` and the API refuses everything but sign-in
/// and billing (`company.source = 'signup_page'`, `auth::paywall`). The web address is kept as asked for;
/// Granite Manager turns it on later, so the company starts on the main
/// host. No session.
///
/// 400 for a missing or bad field or no priced feature picked (also while
/// no feature has a price: sign-up is closed), 409 when the email already signs in
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
    let picked = quote(&priced_features(pool).await?, &input.feature_ids);
    if picked.features.is_empty() {
        return Err(ApiError::BadRequest(
            "Pick at least one feature to pay for".into(),
        ));
    }
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
                              billing_required, source, signed_up_at, signup_ip_hash)
         VALUES (?, ?, ?, 1, 1, ?, ?, ?)",
    )
    .bind(&signup.company_name)
    .bind(&signup.address)
    .bind(&signup.subdomain)
    .bind(crate::auth::paywall::SIGNUP_PAGE)
    .bind(now)
    .bind(&ip_hash)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let company_id = i32::try_from(company_id)
        .map_err(|_| ApiError::Internal("company id out of range".into()))?;
    seed_company(&mut tx, company_id, &signup.subdomain).await?;
    let user_id = sqlx::query(
        "INSERT INTO users (name, email, password, company_id, is_employee, is_admin)
         VALUES (?, ?, ?, ?, 1, 1)",
    )
    .bind(&signup.name)
    .bind(&signup.email)
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
    save_choices(&mut tx, company_id, &picked).await?;
    tx.commit().await?;

    let base = crate::setup_checklist::base_domain(&state);
    send_notices(&state, company_id, &signup, &picked, &base).await;
    tracing::info!(company_id, user_id, "company signed itself up");
    start_session(&state, user_id).await
}

/// Tells Granite Manager about the sign-up and welcomes the new manager.
/// Failures are logged, never shown: the account exists either way.
async fn send_notices(
    state: &AppState,
    company_id: i32,
    signup: &CleanSignup,
    picked: &FeatureQuote,
    base_domain: &str,
) {
    let features = picked
        .features
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let price = format!(
        "${:.2} a month{}",
        cents_to_dollars(picked.total_cents),
        if picked.all_features {
            format!(" (every feature, {}% off)", picked.discount_percent)
        } else {
            String::new()
        }
    );
    let notice = email(
        SIGNUP_NOTICE_TO,
        format!("New sign-up: {}", signup.company_name),
        format!(
            r#"<div style="font-family: Arial, sans-serif;">
  <h2>{company} signed up</h2>
  <p>Company id {company_id}. Manager: {name} &lt;{email}&gt;.</p>
  <p>Web address asked for: <b>{subdomain}.{base}</b> (set the subdomain once DNS and Caddy serve it).</p>
  <p>Address: {address}</p>
  <p>Pays for: {features}, {price}. They are sent to Stripe to pay now; access opens once they have paid.</p>
  <p>Still to set up for them: the web address, a phone line (Telnyx), the email domain, the website lead form and the stones on their website.</p>
</div>"#,
            company = escape_html(&signup.company_name),
            name = escape_html(&signup.name),
            email = escape_html(&signup.email),
            subdomain = escape_html(&signup.subdomain),
            base = escape_html(base_domain),
            address = escape_html(signup.address.as_deref().unwrap_or("not given")),
            features = escape_html(&features),
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
  <p style="color: #475569; line-height: 1.6;">{company} is set up on Granite Manager with {features} for {price}. If you did not finish paying, sign in and pay on the Billing page to open your account.</p>
  <p style="color: #475569; line-height: 1.6;">Your dashboard lists what is left to set up. For a phone line, your company email and your website, our team will help: reply to this email or write to {support}.</p>
  <a href="{app}/login" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Open Granite Manager</a>
</div>"#,
            name = escape_html(&signup.name),
            company = escape_html(&signup.company_name),
            features = escape_html(&features),
            price = escape_html(&price),
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

#[allow(clippy::cast_precision_loss)] // cents of a monthly price
fn cents_to_dollars(cents: i64) -> f64 {
    cents as f64 / 100.0
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
            password: " secret-pass ".into(),
            feature_ids: vec![1, 1, 2],
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
        assert!(refused(|i| i.password = "short".into()));
        assert!(refused(|i| i.accept_terms = false));
        assert!(refused(|i| i.website = Some("http://spam".into())));
    }
}
