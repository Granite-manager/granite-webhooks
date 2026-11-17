//! `/v1/me/gmail`: the user's own Gmail for sending (My Account > Connect
//! Gmail).
//!
//! Once connected, emails the user sends from the CRM go out through their
//! Gmail with Google's send-only permission, From their Gmail address,
//! instead of through Amazon SES (see `gmail_connect`).

use super::account::home_actor;
use crate::app_origin::company_subdomain;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::gmail_connect::{self, GmailError};
use crate::oauth_state;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};

const STATE_KIND: &str = "gmail";
/// The redirect URI registered on Granite Manager's Google app.
const RETURN_PATH: &str = "/redirects/google/gmail-auth";

fn redirect_uri(state: &AppState) -> String {
    format!("{}{RETURN_PATH}", state.app_url.trim_end_matches('/'))
}

fn app(state: &AppState) -> ApiResult<&gmail_connect::GmailApp> {
    state.gmail_app.as_ref().ok_or_else(|| {
        ApiError::BadRequest("Gmail connect is not set up yet. Ask a superuser.".into())
    })
}

/// The company already sends through its own verified Amazon SES domain, so
/// Connect Gmail is not offered there.
async fn company_sends_through_amazon(pool: &sqlx::MySqlPool, company_id: i32) -> ApiResult<bool> {
    let domain: Option<String> =
        sqlx::query_scalar("SELECT NULLIF(TRIM(domain), '') FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(match domain {
        Some(domain) => common::amazon::email::domain_verified(&domain)
            .await
            .unwrap_or(false),
        None => false,
    })
}

fn amazon_refusal() -> ApiError {
    ApiError::BadRequest(
        "Your company already sends email from its own domain, so Gmail can't be connected.".into(),
    )
}

/// `GET /v1/me/gmail`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyGmail {
    /// Granite Manager's Google app is set up and the company does not send
    /// through its own Amazon SES domain, so Connect Gmail works. Stays true
    /// while the caller is connected, so they can disconnect.
    pub available: bool,
    /// The connected Gmail address; null when not connected.
    pub email: Option<String>,
}

/// My Gmail
///
/// Whether the caller's emails go out through their own Gmail, and which.
#[crate::openapi::path(
    get,
    path = "/v1/me/gmail",
    operation_id = "get_my_gmail",
    tag = "me",
    responses(
        (status = 200, description = "The caller's Gmail connection", body = MyGmail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn status(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<MyGmail>> {
    let actor = home_actor(&state.pool, user.id).await?;
    let email = gmail_connect::connected_email(&state.pool, actor.user_id).await?;
    let available = state.gmail_app.is_some()
        && (email.is_some()
            || !company_sends_through_amazon(&state.pool, actor.company_id).await?);
    Ok(Json(MyGmail { available, email }))
}

/// `POST /v1/me/gmail/start`.
#[derive(Debug, Serialize, ToSchema)]
pub struct GmailSignIn {
    /// Google's sign-in page.
    pub url: String,
}

/// Start connecting my Gmail
///
/// Google's sign-in page for the caller, asking for send-only Gmail access.
/// Google returns to `/redirects/google/gmail-auth` with `code` and `state`
/// (or `error`); post them to `POST /v1/me/gmail` within 15 minutes.
#[crate::openapi::path(
    post,
    path = "/v1/me/gmail/start",
    operation_id = "start_my_gmail",
    tag = "me",
    responses(
        (status = 200, description = "Google's sign-in page", body = GmailSignIn),
        (status = 400, description = "Gmail connect is not set up, or the company sends through its own Amazon domain", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<GmailSignIn>> {
    let actor = home_actor(&state.pool, user.id).await?;
    let app = app(&state)?;
    if company_sends_through_amazon(&state.pool, actor.company_id).await? {
        return Err(amazon_refusal());
    }
    let state_token = oauth_state::sign(
        &state.session_secret,
        STATE_KIND,
        actor.company_id,
        actor.user_id,
    );
    Ok(Json(GmailSignIn {
        url: app.authorize_url(&redirect_uri(&state), &state_token),
    }))
}

/// Body of `POST /v1/me/gmail`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct GmailReturn {
    /// `state` as Google returned it.
    pub state: String,
    /// `code` as Google returned it; absent when the user cancelled.
    #[serde(default)]
    pub code: Option<String>,
    /// `error` as Google returned it (the user cancelled).
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /v1/me/gmail`.
#[derive(Debug, Serialize, ToSchema)]
pub struct GmailConnected {
    /// The Gmail address the caller's emails go out from now.
    pub email: String,
    /// The company's subdomain, to go back to My Account there; null when it
    /// has none.
    pub company_subdomain: Option<String>,
}

/// Finish connecting my Gmail
///
/// Trades the code for the caller's Gmail tokens. From then on their emails
/// go out through that Gmail.
#[crate::openapi::path(
    post,
    path = "/v1/me/gmail",
    operation_id = "finish_my_gmail",
    tag = "me",
    request_body = GmailReturn,
    responses(
        (status = 200, description = "Connected", body = GmailConnected),
        (status = 400, description = "Cancelled, expired or foreign state, send permission not given, or Google refused the code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<GmailReturn>,
) -> ApiResult<Json<GmailConnected>> {
    if input
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty())
    {
        return Err(ApiError::BadRequest(
            "Gmail connection was cancelled.".into(),
        ));
    }
    let pool = &state.pool;
    let actor = home_actor(pool, user.id).await?;
    if !oauth_state::verify(
        &state.session_secret,
        &input.state,
        STATE_KIND,
        actor.company_id,
        actor.user_id,
    ) {
        return Err(ApiError::BadRequest(
            "Gmail connection expired. Please try again.".into(),
        ));
    }
    let Some(code) = input
        .code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return Err(ApiError::BadRequest(
            "Invalid data returned from Google.".into(),
        ));
    };
    let email = connect(
        &state,
        actor.company_id,
        actor.user_id,
        code,
        &redirect_uri(&state),
    )
    .await?;
    Ok(Json(GmailConnected {
        email,
        company_subdomain: company_subdomain(pool, actor.company_id).await?,
    }))
}

/// Trades a Google code for the user's Gmail tokens and saves them.
async fn connect(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    code: &str,
    redirect_uri: &str,
) -> ApiResult<String> {
    if company_sends_through_amazon(&state.pool, company_id).await? {
        return Err(amazon_refusal());
    }
    match app(state)?
        .connect(
            &state.pool,
            &state.telephony_http,
            company_id,
            user_id,
            code,
            redirect_uri,
        )
        .await
    {
        Ok(email) => Ok(email),
        Err(GmailError::Database(error)) => Err(ApiError::Database(error)),
        Err(GmailError::NoSendPermission) => Err(ApiError::BadRequest(
            "Google didn't give permission to send email. Connect again and tick \"Send email on your behalf\".".into(),
        )),
        Err(error) => {
            tracing::warn!(%error, user_id, "Gmail connect failed");
            Err(ApiError::BadRequest(
                "Could not finish the Gmail connection. Try again.".into(),
            ))
        }
    }
}

/// Body of `POST /v1/me/gmail/app`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct GmailAppSignIn {
    /// The `serverAuthCode` Google's own sign-in in the Granite Manager app
    /// returned (issued for Granite Manager's web client, with send-only
    /// Gmail access).
    pub server_auth_code: String,
}

/// `POST /v1/me/gmail/app`.
#[derive(Debug, Serialize, ToSchema)]
pub struct GmailAppConnected {
    /// The Gmail address the caller's emails go out from now.
    pub email: String,
}

/// Connect my Gmail from the app
///
/// The Granite Manager app signs in with Google's own sign-in (no web page)
/// and posts the code it got. Google sent the code to the app itself, so
/// there is no `state` to check; it is traded with no redirect URI.
#[crate::openapi::path(
    post,
    path = "/v1/me/gmail/app",
    operation_id = "connect_my_gmail_from_app",
    tag = "me",
    request_body = GmailAppSignIn,
    responses(
        (status = 200, description = "Connected", body = GmailAppConnected),
        (status = 400, description = "Gmail connect is not set up, send permission not given, or Google refused the code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn app_finish(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<GmailAppSignIn>,
) -> ApiResult<Json<GmailAppConnected>> {
    let code = input.server_auth_code.trim();
    if code.is_empty() {
        return Err(ApiError::BadRequest(
            "Invalid data returned from Google.".into(),
        ));
    }
    let actor = home_actor(&state.pool, user.id).await?;
    let email = connect(&state, actor.company_id, actor.user_id, code, "").await?;
    Ok(Json(GmailAppConnected { email }))
}

/// Disconnect my Gmail
///
/// Ends the caller's Gmail connection; their emails go out through Granite
/// Manager's sending again.
#[crate::openapi::path(
    delete,
    path = "/v1/me/gmail",
    operation_id = "disconnect_my_gmail",
    tag = "me",
    responses(
        (status = 204, description = "Disconnected (or was not connected)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn disconnect(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<StatusCode> {
    let actor = home_actor(&state.pool, user.id).await?;
    match &state.gmail_app {
        Some(app) => {
            app.disconnect(&state.pool, &state.telephony_http, actor.user_id)
                .await?;
        }
        None => {
            sqlx::query("DELETE FROM gmail_user_logins WHERE user_id = ?")
                .bind(actor.user_id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
