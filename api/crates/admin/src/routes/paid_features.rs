//! Which companies have paid for which features, and how much each company
//! uses them (ENG-58, ENG-59). Superusers only; shown on the superuser
//! Payments screen.
//!
//! - `GET    /v1/superuser/paid-features`          → features, who paid, usage
//! - `POST   /v1/superuser/paid-features`          → add a feature
//! - `PUT    /v1/superuser/paid-features/{id}`     → rename it or change its usage count
//! - `DELETE /v1/superuser/paid-features/{id}`     → remove it (and its marks)
//! - `PUT    /v1/superuser/companies/{id}/paid-features/{feature_id}` → mark paid
//! - `DELETE /v1/superuser/companies/{id}/paid-features/{feature_id}` → unmark
//!
//! "Paid" is bookkeeping only: nothing here grants or takes away access.
//! Access stays with `company.<feature>_enabled` and `global_feature_flags`,
//! so the screen can show a company using a feature it has not paid for.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_paid_features, create_feature))
        .routes(routes!(update_feature, delete_feature))
        .routes(routes!(mark_paid, unmark_paid))
}

/// Usage is counted over this many days back from now.
pub const USAGE_DAYS: i64 = 30;

/// A usage count a feature can show.
///
/// `sql` returns `(company_id, n)` rows for everything since its one `?`
/// parameter. Each reads one table by its own `company_id`, grouped, so the
/// whole screen costs one query per metric.
pub struct Metric {
    pub key: &'static str,
    /// What is counted, as the screen says it ("42 texts sent").
    pub label: &'static str,
    sql: &'static str,
}

pub static METRICS: [Metric; 10] = [
    Metric {
        key: "quickbooks_invoices",
        label: "invoices sent to QuickBooks",
        sql: "SELECT company_id, COUNT(*) FROM event_qbo_invoices
               WHERE created_at >= ? GROUP BY company_id",
    },
    Metric {
        key: "signature_requests",
        label: "documents sent for signature",
        sql: "SELECT company_id, COUNT(*) FROM signature_requests
               WHERE created_at >= ? AND status <> 'draft' GROUP BY company_id",
    },
    Metric {
        key: "projects_created",
        label: "projects created",
        sql: "SELECT company_id, COUNT(*) FROM projects
               WHERE created_at >= ? GROUP BY company_id",
    },
    Metric {
        key: "phone_calls",
        label: "calls",
        sql: "SELECT company_id, COUNT(*) FROM phone_calls
               WHERE started_at >= ? GROUP BY company_id",
    },
    Metric {
        key: "texts_sent",
        label: "texts sent",
        sql: "SELECT company_id, COUNT(*) FROM telnyx_sms
               WHERE created_date >= ? AND direction = 'outbound' AND is_echo = 0
                 AND status <> 'failed'
               GROUP BY company_id",
    },
    Metric {
        key: "text_flow_enrollments",
        label: "customers put in a text flow",
        sql: "SELECT company_id, COUNT(*) FROM sms_flow_enrollments
               WHERE started_at >= ? GROUP BY company_id",
    },
    Metric {
        key: "chat_questions",
        label: "Chat questions",
        sql: "SELECT company_id, COUNT(*) FROM chat_log
               WHERE created_at >= ? AND company_id IS NOT NULL GROUP BY company_id",
    },
    // Until 2026-11-05 an AI reply was logged as `quick_reply` with the
    // composer suggestions; only AI reply asked the large model (as in the
    // onboarding usage check `ai_reply_used`).
    Metric {
        key: "ai_replies",
        label: "AI reply drafts",
        sql: "SELECT company_id, COUNT(*) FROM ai_usage_log
               WHERE created_at >= ? AND success = 1 AND company_id IS NOT NULL
                 AND (feature = 'ai_reply'
                      OR (feature = 'quick_reply' AND model LIKE 'gpt-6.1-sol%'))
               GROUP BY company_id",
    },
    Metric {
        key: "ai_call_notes",
        label: "calls turned into notes",
        sql: "SELECT company_id, COUNT(*) FROM ai_usage_log
               WHERE created_at >= ? AND success = 1 AND company_id IS NOT NULL
                 AND feature IN ('call_note_summary', 'call_activity_extract')
               GROUP BY company_id",
    },
    Metric {
        key: "call_coaching",
        label: "calls coached",
        sql: "SELECT company_id, COUNT(*) FROM call_coaching_analyses
               WHERE created_at >= ? GROUP BY company_id",
    },
];

fn metric_exists(key: &str) -> bool {
    METRICS.iter().any(|metric| metric.key == key)
}

/// A feature a company can pay for.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Feature {
    /// The feature's id, used in the paid-features routes.
    pub id: i32,
    /// Its name as the superuser screen shows it, e.g. "E-signatures".
    pub name: String,
    /// The usage count shown for it (a `metrics` key); null: none.
    pub usage_metric: Option<String>,
}

/// A usage count the API knows how to compute.
#[derive(Debug, Serialize, ToSchema)]
pub struct MetricInfo {
    /// The value a feature's `usage_metric` takes to show this count.
    pub key: String,
    /// What is counted, e.g. "texts sent".
    pub label: String,
}

/// A company that has paid for a feature.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PaidMark {
    /// The company.
    pub company_id: i32,
    /// The feature it has paid for.
    pub feature_id: i32,
}

/// How much a company used a metric in the last `usage_days` days. Companies
/// with no use are left out.
#[derive(Debug, Serialize, ToSchema)]
pub struct Usage {
    /// The company.
    pub company_id: i32,
    /// A `metrics` key.
    pub metric: String,
    /// How many times, at least 1.
    pub count: i64,
}

/// `GET /v1/superuser/paid-features`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaidFeatures {
    /// Every feature, by name.
    pub features: Vec<Feature>,
    /// Every usage count a feature can show.
    pub metrics: Vec<MetricInfo>,
    /// Which company paid for which feature (deleted companies left out).
    pub paid: Vec<PaidMark>,
    /// Usage of the metrics the features show, per company; no row: not used.
    pub usage: Vec<Usage>,
    /// The days the usage covers, back from now.
    pub usage_days: i64,
}

const fn superuser(is_superuser: bool) -> ApiResult<()> {
    if is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Features, who paid for them, and usage
///
/// Every feature, the companies marked as having paid for each, and every
/// company's usage counts over the last `usage_days` days. Superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/paid-features",
    operation_id = "list_paid_features",
    tag = "superuser",
    responses(
        (status = 200, description = "Features, paid marks and usage", body = PaidFeatures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_paid_features(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<PaidFeatures>> {
    superuser(user.is_superuser)?;
    // One connection for the whole screen, queries one after another.
    let mut conn = state.pool.acquire().await?;
    let features: Vec<Feature> =
        sqlx::query_as("SELECT id, name, usage_metric FROM features ORDER BY name, id")
            .fetch_all(&mut *conn)
            .await?;
    let paid: Vec<PaidMark> = sqlx::query_as(
        "SELECT cf.company_id, cf.feature_id
           FROM company_features cf
           JOIN company c ON c.id = cf.company_id AND c.deleted_at IS NULL",
    )
    .fetch_all(&mut *conn)
    .await?;
    let since = (Utc::now() - Duration::days(USAGE_DAYS)).naive_utc();
    let mut usage = Vec::new();
    // Only the metrics some feature shows.
    for metric in METRICS.iter().filter(|metric| {
        features
            .iter()
            .any(|feature| feature.usage_metric.as_deref() == Some(metric.key))
    }) {
        let rows: Vec<(i32, i64)> = sqlx::query_as(metric.sql)
            .bind(since)
            .fetch_all(&mut *conn)
            .await?;
        usage.extend(rows.into_iter().map(|(company_id, count)| Usage {
            company_id,
            metric: metric.key.to_string(),
            count,
        }));
    }
    Ok(Json(PaidFeatures {
        features,
        metrics: METRICS
            .iter()
            .map(|metric| MetricInfo {
                key: metric.key.to_string(),
                label: metric.label.to_string(),
            })
            .collect(),
        paid,
        usage,
        usage_days: USAGE_DAYS,
    }))
}

/// Body of `POST /v1/superuser/paid-features` and `PUT .../{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FeatureInput {
    /// 1 to 128 characters, unique.
    pub name: String,
    /// A `metrics` key, or null for no usage count.
    #[serde(default)]
    pub usage_metric: Option<String>,
}

fn checked(input: FeatureInput) -> ApiResult<(String, Option<String>)> {
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 128 {
        return Err(ApiError::BadRequest(
            "The name must be 1 to 128 characters".into(),
        ));
    }
    let metric = input
        .usage_metric
        .map(|metric| metric.trim().to_string())
        .filter(|metric| !metric.is_empty());
    if let Some(metric) = &metric
        && !metric_exists(metric)
    {
        return Err(ApiError::BadRequest(format!(
            "Unknown usage count: {metric}"
        )));
    }
    Ok((name, metric))
}

fn duplicate_name(error: sqlx::Error) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("A feature with this name already exists".into())
        }
        _ => error.into(),
    }
}

/// Add a feature
///
/// Superusers only.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/paid-features",
    operation_id = "create_paid_feature",
    tag = "superuser",
    request_body = FeatureInput,
    responses(
        (status = 201, description = "Added", body = Feature),
        (status = 400, description = "Bad name or usage count", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_feature(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<FeatureInput>,
) -> ApiResult<(StatusCode, Json<Feature>)> {
    superuser(user.is_superuser)?;
    let (name, usage_metric) = checked(input)?;
    let id = sqlx::query("INSERT INTO features (name, usage_metric) VALUES (?, ?)")
        .bind(&name)
        .bind(&usage_metric)
        .execute(&state.pool)
        .await
        .map_err(duplicate_name)?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(Feature {
            id: i32::try_from(id)
                .map_err(|_| ApiError::Internal("Feature id out of range".into()))?,
            name,
            usage_metric,
        }),
    ))
}

/// Rename a feature or change its usage count
///
/// Superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/paid-features/{id}",
    operation_id = "update_paid_feature",
    tag = "superuser",
    params(("id" = i32, Path, description = "Feature id")),
    request_body = FeatureInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Bad name or usage count", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such feature", body = ErrorBody),
        (status = 409, description = "The name is taken", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_feature(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<FeatureInput>,
) -> ApiResult<StatusCode> {
    superuser(user.is_superuser)?;
    let (name, usage_metric) = checked(input)?;
    sqlx::query("UPDATE features SET name = ?, usage_metric = ? WHERE id = ?")
        .bind(&name)
        .bind(&usage_metric)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(duplicate_name)?;
    // An unchanged row reports 0 rows affected: look it up instead.
    let exists: Option<i32> = sqlx::query_scalar("SELECT id FROM features WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    exists.ok_or(ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a feature
///
/// Removes the companies' paid marks for it too. Superusers only.
#[crate::openapi::path(
    delete,
    path = "/v1/superuser/paid-features/{id}",
    operation_id = "delete_paid_feature",
    tag = "superuser",
    params(("id" = i32, Path, description = "Feature id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such feature", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_feature(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    superuser(user.is_superuser)?;
    let deleted = sqlx::query("DELETE FROM features WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Mark a company as having paid for a feature
///
/// Marking it again changes nothing. Superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/superuser/companies/{id}/paid-features/{feature_id}",
    operation_id = "mark_company_feature_paid",
    tag = "superuser",
    params(
        ("id" = i32, Path, description = "Company id"),
        ("feature_id" = i32, Path, description = "Feature id"),
    ),
    responses(
        (status = 204, description = "Marked"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company or feature", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_paid(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, feature_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    superuser(user.is_superuser)?;
    let inserted = sqlx::query(
        "INSERT IGNORE INTO company_features (company_id, feature_id, created_by_user_id)
         SELECT c.id, f.id, ?
           FROM company c JOIN features f ON f.id = ?
          WHERE c.id = ? AND c.deleted_at IS NULL",
    )
    .bind(user.id)
    .bind(feature_id)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    if inserted.rows_affected() == 0 {
        // Already marked, or no such company or feature.
        let marked: Option<i32> = sqlx::query_scalar(
            "SELECT cf.company_id FROM company_features cf
               JOIN company c ON c.id = cf.company_id AND c.deleted_at IS NULL
              WHERE cf.company_id = ? AND cf.feature_id = ?",
        )
        .bind(company_id)
        .bind(feature_id)
        .fetch_optional(&state.pool)
        .await?;
        marked.ok_or(ApiError::NotFound)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Unmark a company's paid feature
///
/// Unmarking one that is not marked changes nothing. Superusers only.
#[crate::openapi::path(
    delete,
    path = "/v1/superuser/companies/{id}/paid-features/{feature_id}",
    operation_id = "unmark_company_feature_paid",
    tag = "superuser",
    params(
        ("id" = i32, Path, description = "Company id"),
        ("feature_id" = i32, Path, description = "Feature id"),
    ),
    responses(
        (status = 204, description = "Unmarked"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unmark_paid(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((company_id, feature_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    superuser(user.is_superuser)?;
    sqlx::query("DELETE FROM company_features WHERE company_id = ? AND feature_id = ?")
        .bind(company_id)
        .bind(feature_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
