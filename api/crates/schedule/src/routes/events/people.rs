//! People added to an event: the company's users who take part in it,
//! besides the installer or template maker it is assigned to.
//!
//! - `GET /v1/schedule/team` → the people who can be added
//! - `people_ids` on `POST /v1/events` and `PUT /v1/events/{id}` saves them
//! - `people` on every event of `GET /v1/schedule/events`,
//!   `/v1/schedule/unscheduled` and `/v1/me/schedule`

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::schedule::board::ScheduleEvent;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use sqlx::MySqlPool;
use std::collections::{BTreeSet, HashMap};

/// Most people one event may have.
pub const MAX_EVENT_PEOPLE: usize = 50;

/// `positions.id` of a check-in kiosk account, never a person.
const KIOSK: i32 = 8;

/// A person on an event, or one who can be added.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EventPerson {
    /// User id.
    pub id: i32,
    /// Name ("User {id}" when blank).
    pub name: String,
    /// Their own picture, when they added one.
    pub photo_url: Option<String>,
}

fn person(id: i32, name: Option<String>, photo_url: Option<String>) -> EventPerson {
    EventPerson {
        id,
        name: name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("User {id}")),
        photo_url: photo_url.filter(|url| !url.trim().is_empty()),
    }
}

/// The company's people, by name: its live users who work for it (employees,
/// admins, anyone holding a position there), less check-in kiosks.
async fn company_team(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<EventPerson>> {
    let rows: Vec<(i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT u.id, u.name, u.photo_url FROM users u
          WHERE u.is_deleted = 0
            AND ((u.company_id = ? AND (u.is_employee = 1 OR u.is_admin = 1))
              OR EXISTS (SELECT 1 FROM users_positions up
                          WHERE up.user_id = u.id AND up.company_id = ?))
            AND NOT EXISTS (SELECT 1 FROM users_positions kp
                             WHERE kp.user_id = u.id AND kp.position_id = ?)
          ORDER BY u.name, u.id",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(KIOSK)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, photo)| person(id, name, photo))
        .collect())
}

/// People who can be added to an event
///
/// The company's live employees and admins by name, with their pictures
/// (check-in kiosks left out). Send their ids as `people_ids` with
/// `POST /v1/events` or `PUT /v1/events/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/team",
    operation_id = "list_schedule_team",
    tag = "schedule",
    responses(
        (status = 200, description = "People", body = Vec<EventPerson>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn team(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Vec<EventPerson>>> {
    Ok(Json(company_team(&state.pool, user.company_id).await?))
}

/// The ids, each once, checked against the company's people.
pub async fn check(pool: &MySqlPool, company_id: i32, ids: &[i32]) -> ApiResult<Vec<i32>> {
    let ids: BTreeSet<i32> = ids.iter().copied().filter(|id| *id > 0).collect();
    if ids.len() > MAX_EVENT_PEOPLE {
        return Err(ApiError::BadRequest(format!(
            "An event can have at most {MAX_EVENT_PEOPLE} people"
        )));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let team = company_team(pool, company_id).await?;
    if ids
        .iter()
        .any(|id| !team.iter().any(|person| person.id == *id))
    {
        return Err(ApiError::BadRequest("Person not found".into()));
    }
    Ok(ids.into_iter().collect())
}

/// The ids of the people on an event.
async fn saved_ids(pool: &MySqlPool, event_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar("SELECT user_id FROM event_people WHERE event_id = ? ORDER BY user_id")
        .bind(event_id)
        .fetch_all(pool)
        .await
}

/// Puts exactly these (checked) people on the event; the history line of
/// the change, when anything changed.
pub async fn save(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
    ids: &[i32],
) -> sqlx::Result<Option<String>> {
    let before = saved_ids(pool, event_id).await?;
    let added: Vec<i32> = ids
        .iter()
        .copied()
        .filter(|id| !before.contains(id))
        .collect();
    let removed: Vec<i32> = before
        .iter()
        .copied()
        .filter(|id| !ids.contains(id))
        .collect();
    if added.is_empty() && removed.is_empty() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    for id in &removed {
        sqlx::query("DELETE FROM event_people WHERE event_id = ? AND user_id = ?")
            .bind(event_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    for id in &added {
        sqlx::query(
            "INSERT IGNORE INTO event_people (event_id, user_id, company_id) VALUES (?, ?, ?)",
        )
        .bind(event_id)
        .bind(id)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let names = names(pool, &[added.as_slice(), removed.as_slice()].concat()).await?;
    let list = |ids: &[i32]| {
        ids.iter()
            .map(|id| {
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("User {id}"))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("Added {}", list(&added)));
    }
    if !removed.is_empty() {
        let verb = if added.is_empty() {
            "Removed"
        } else {
            "removed"
        };
        parts.push(format!("{verb} {}", list(&removed)));
    }
    Ok(Some(parts.join("; ")))
}

async fn names(pool: &MySqlPool, ids: &[i32]) -> sqlx::Result<HashMap<i32, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut sql =
        sqlx::QueryBuilder::<sqlx::MySql>::new("SELECT id, name FROM users WHERE id IN (");
    let mut list = sql.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    sql.push(")");
    let rows: Vec<(i32, Option<String>)> = sql.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| (id, person(id, name, None).name))
        .collect())
}

/// Puts their people on the events, by name.
pub async fn attach<'a>(
    pool: &MySqlPool,
    events: impl IntoIterator<Item = &'a mut ScheduleEvent>,
) -> sqlx::Result<()> {
    let events: Vec<&mut ScheduleEvent> = events.into_iter().collect();
    if events.is_empty() {
        return Ok(());
    }
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT ep.event_id, u.id, u.name, u.photo_url FROM event_people ep
           JOIN users u ON u.id = ep.user_id AND u.is_deleted = 0
          WHERE ep.event_id IN (",
    );
    let mut ids = sql.separated(", ");
    for event in &events {
        ids.push_bind(event.id);
    }
    sql.push(") ORDER BY u.name, u.id");
    let rows: Vec<(i32, i32, Option<String>, Option<String>)> =
        sql.build_query_as().fetch_all(pool).await?;
    let mut people: HashMap<i32, Vec<EventPerson>> = HashMap::new();
    for (event, id, name, photo) in rows {
        people
            .entry(event)
            .or_default()
            .push(person(id, name, photo));
    }
    for event in events {
        event.people = people.remove(&event.id).unwrap_or_default();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::person;

    #[test]
    fn a_blank_name_reads_as_the_user() {
        assert_eq!(person(7, Some("  ".into()), None).name, "User 7");
        assert_eq!(person(7, Some(" Liza ".into()), None).name, "Liza");
        assert_eq!(person(7, None, Some(" ".into())).photo_url, None);
    }
}
