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

/// `GET /v1/me/gmail`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyGmail {
    /// Granite Manager's Google app is set up, so Connect Gmail works.
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
    Ok(Json(MyGmail {
        available: state.gmail_app.is_some(),
        email: gmail_connect::connected_email(&state.pool, actor.user_id).await?,
    }))
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
        (status = 400, description = "Gmail connect is not set up", body = ErrorBody),
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
    let email = match app(&state)?
        .connect(
            pool,
            &state.telephony_http,
            actor.company_id,
            actor.user_id,
            code,
            &redirect_uri(&state),
        )
        .await
    {
        Ok(email) => email,
        Err(GmailError::Database(error)) => return Err(ApiError::Database(error)),
        Err(GmailError::NoSendPermission) => {
            return Err(ApiError::BadRequest(
                "Google didn't give permission to send email. Connect again and tick \"Send email on your behalf\".".into(),
            ));
        }
        Err(error) => {
            tracing::warn!(%error, user_id = actor.user_id, "Gmail connect failed");
            return Err(ApiError::BadRequest(
                "Could not finish the Gmail connection. Try again.".into(),
            ));
        }
    };
    Ok(Json(GmailConnected {
        email,
        company_subdomain: company_subdomain(pool, actor.company_id).await?,
    }))
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
