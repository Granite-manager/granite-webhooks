//! `GET /v1/customers/{id}/files`: files kept on a customer. GMQ TOPS's
//! came from Slabware (invoices, signed contracts and uploaded files); the
//! customer page shows them under the `QuickBooks` invoices.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

/// A file kept on a customer.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CustomerFile {
    /// The file.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// `invoice`, `signed_contract`, `signed_invoice` or `upload`.
    pub kind: String,
    /// What it belongs to, e.g. `Quote #11934` or `Invoice #11162`.
    pub label: Option<String>,
    /// Its date (`YYYY-MM-DD`).
    pub file_date: Option<String>,
    /// Where to open it.
    pub url: String,
}

/// List a customer's files
///
/// Files kept on the customer, newest first.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/files",
    operation_id = "list_customer_files",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "Files", body = [CustomerFile]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn files(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(customer_id): Path<i32>,
) -> ApiResult<Json<Vec<CustomerFile>>> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    let files: Vec<CustomerFile> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, name, kind, label,
                DATE_FORMAT(file_date, '%Y-%m-%d') AS file_date, url
           FROM customer_files WHERE company_id = ? AND customer_id = ?
          ORDER BY file_date DESC, id DESC",
    )
    .bind(user.company_id)
    .bind(customer_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(files))
}
