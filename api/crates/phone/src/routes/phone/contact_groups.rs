//! Contact groups in the app's Contacts (like the built-in Coworkers):
//! `/v1/phone/contact-groups`. A group made by a Manager, a Sales Manager, an
//! admin or a superuser is the company's (`shared`): every user of the
//! company sees it and its members, and only those people rename, delete or
//! refill it. A group made by anyone else is their own: only they see and
//! change it. Hiding a group (or Coworkers) from the top of Contacts is each
//! user's own.

use super::bad_request;
use crate::auth::device::DeviceUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder, Transaction};
use std::collections::{BTreeSet, HashMap};

/// The longest group name, in characters.
pub const MAX_NAME_CHARS: usize = 60;
/// The most groups a user may see: the company's and their own.
pub const MAX_GROUPS: i64 = 100;
/// Sales Manager and Manager: with admins and superusers, they make and
/// change the company's groups.
const SHARING_POSITIONS: [i32; 2] = [2, 5];
/// The most members (customers and coworkers together) a group may have.
pub const MAX_MEMBERS: usize = 500;

/// One of the company's contact groups, as I see it.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactGroup {
    /// The group's id.
    pub id: i64,
    /// Its name, e.g. "Installers" (1 to 60 characters).
    pub name: String,
    /// I hid it from the top of my Contacts (it still exists, with its
    /// members, and other users still see it at their top).
    pub hidden: bool,
    /// The customers in it (ids of `GET /v1/phone/contacts/index`
    /// `contacts`), ascending. Deleted customers are left out.
    pub customer_ids: Vec<i32>,
    /// The coworkers in it (user ids, as `coworkers[].userId` of the
    /// contact index), ascending. Deleted users are left out; I may be one of
    /// them.
    pub user_ids: Vec<i32>,
    /// The user who made the group; null once that user is gone.
    pub created_by: Option<i32>,
    /// True: the company's group, every user sees it. False: my own group,
    /// only I see it.
    pub shared: bool,
    /// I may rename it, delete it and change who is in it: always for my
    /// own group; for the company's only a Manager, a Sales Manager, an
    /// admin or a superuser.
    pub can_edit: bool,
}

/// `GET /v1/phone/contact-groups`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContactGroups {
    /// I hid the built-in Coworkers group (everyone at the company with a
    /// phone number) from the top of my Contacts.
    pub coworkers_hidden: bool,
    /// A group I make now is the company's, seen by every user (I am a
    /// Manager, a Sales Manager, an admin or a superuser). False: it is my
    /// own, seen only by me.
    pub can_share: bool,
    /// The company's groups and my own, by name.
    pub groups: Vec<ContactGroup>,
}

/// Who is in a group: customers and coworkers of my company.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupMembers {
    /// Customer ids of my company's live customers (400 `unknown_contact`
    /// for any other id, or a deleted customer). Repeats count once.
    #[serde(default)]
    pub customer_ids: Vec<i32>,
    /// User ids of my company's users (400 `unknown_coworker` for any other
    /// id, or a deleted user). Repeats count once.
    #[serde(default)]
    pub user_ids: Vec<i32>,
}

/// Body of `POST /v1/phone/contact-groups`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewGroup {
    /// Its name, 1 to 60 characters (trimmed); unique in the company,
    /// ignoring case (400 `name_taken`).
    pub name: String,
    /// Customer ids to put in it at once (as `PUT .../members`).
    #[serde(default)]
    pub customer_ids: Vec<i32>,
    /// Coworkers' user ids to put in it at once (as `PUT .../members`).
    #[serde(default)]
    pub user_ids: Vec<i32>,
}

/// Body of `PATCH /v1/phone/contact-groups/{id}`: what to change; a field
/// left out stays as it is.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupChange {
    /// A new name, as for `POST /v1/phone/contact-groups`.
    #[serde(default)]
    pub name: Option<String>,
    /// True hides the group from the top of my Contacts, false shows it
    /// again. Only for me: other users' tops are left alone.
    #[serde(default)]
    pub hidden: Option<bool>,
}

/// Body of `PATCH /v1/phone/contact-groups/coworkers`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoworkersChange {
    /// True hides Coworkers from the top of my Contacts, false shows it
    /// again. Only for me.
    pub hidden: bool,
}

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: i64,
    name: String,
    hidden: bool,
    created_by: Option<i32>,
    shared: bool,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: i64,
    group_id: i64,
    customer_id: Option<i32>,
    user_id: Option<i32>,
}

/// The user makes and changes the company's groups: a superuser, an admin,
/// or a Sales Manager or Manager of the company.
async fn may_share(pool: &MySqlPool, user: &SessionUser) -> sqlx::Result<bool> {
    if user.is_superuser || user.is_admin {
        return Ok(true);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users_positions
                        WHERE user_id = ? AND company_id = ? AND position_id IN (?, ?))",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(SHARING_POSITIONS[0])
    .bind(SHARING_POSITIONS[1])
    .fetch_one(pool)
    .await
}

/// The groups `me` sees (the company's and their own; one, with `only`), by
/// name. `sharer`: `me` may change the company's groups.
async fn load(
    pool: &MySqlPool,
    company_id: i32,
    me: i32,
    sharer: bool,
    only: Option<i64>,
) -> sqlx::Result<Vec<ContactGroup>> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT g.id, g.name, h.id IS NOT NULL AS hidden, g.created_by, g.shared <> 0 AS shared
           FROM phone_contact_groups g
           LEFT JOIN phone_contact_group_hidden h ON h.group_id = g.id AND h.user_id = ",
    );
    query
        .push_bind(me)
        .push(" WHERE g.company_id = ")
        .push_bind(company_id)
        .push(" AND (g.shared <> 0 OR g.created_by = ")
        .push_bind(me)
        .push(")");
    if let Some(id) = only {
        query.push(" AND g.id = ").push_bind(id);
    }
    query.push(" ORDER BY g.name, g.id");
    let rows: Vec<GroupRow> = query.build_query_as().fetch_all(pool).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT m.id, m.group_id, m.customer_id, m.user_id
           FROM phone_contact_group_members m
           LEFT JOIN customers c ON c.id = m.customer_id
           LEFT JOIN users u ON u.id = m.user_id
          WHERE ((m.customer_id IS NOT NULL AND c.deleted_at IS NULL AND c.company_id = ",
    );
    query
        .push_bind(company_id)
        .push(") OR (m.user_id IS NOT NULL AND u.is_deleted = 0 AND u.company_id = ")
        .push_bind(company_id)
        .push("))");
    crate::sql::push_in(&mut query, "m.group_id", &ids);
    query.push(" ORDER BY m.customer_id, m.user_id");
    let members: Vec<MemberRow> = query.build_query_as().fetch_all(pool).await?;
    let mut by_group: HashMap<i64, (Vec<i32>, Vec<i32>)> = HashMap::new();
    for member in members {
        let entry = by_group.entry(member.group_id).or_default();
        match (member.customer_id, member.user_id) {
            (Some(customer), _) => entry.0.push(customer),
            (None, Some(user)) => entry.1.push(user),
            (None, None) => {}
        }
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let (customer_ids, user_ids) = by_group.remove(&row.id).unwrap_or_default();
            ContactGroup {
                id: row.id,
                name: row.name,
                hidden: row.hidden,
                customer_ids,
                user_ids,
                created_by: row.created_by,
                shared: row.shared,
                can_edit: if row.shared {
                    sharer
                } else {
                    row.created_by == Some(me)
                },
            }
        })
        .collect())
}

/// One group as I see it; 404 when there is none (another company's, or
/// another user's own).
async fn one(pool: &MySqlPool, device: &DeviceUser, id: i64) -> ApiResult<ContactGroup> {
    let sharer = may_share(pool, &device.user).await?;
    load(
        pool,
        device.user.company_id,
        device.user.id,
        sharer,
        Some(id),
    )
    .await?
    .pop()
    .ok_or(ApiError::NotFound)
}

/// [`one`], when I may change it; 403 for the company's group when I am not
/// a Manager, a Sales Manager, an admin or a superuser.
async fn editable(pool: &MySqlPool, device: &DeviceUser, id: i64) -> ApiResult<ContactGroup> {
    let group = one(pool, device, id).await?;
    if group.can_edit {
        Ok(group)
    } else {
        Err(ApiError::Forbidden)
    }
}

/// A group name as stored; 400 when it is empty or too long.
fn checked_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::BadRequest(
            "Give the group a name of at most 60 characters.".into(),
        ));
    }
    Ok(name.to_string())
}

/// Sorted, without repeats.
fn distinct(ids: &[i32]) -> Vec<i32> {
    ids.iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The members, checked against the company: 400 `unknown_contact`,
/// `unknown_coworker` or `too_many_members`.
async fn checked_members(
    pool: &MySqlPool,
    company_id: i32,
    customer_ids: &[i32],
    user_ids: &[i32],
) -> ApiResult<(Vec<i32>, Vec<i32>)> {
    let customers = distinct(customer_ids);
    let users = distinct(user_ids);
    if customers.len() + users.len() > MAX_MEMBERS {
        return Err(bad_request("too_many_members"));
    }
    if !customers.is_empty() {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT COUNT(*) FROM customers WHERE deleted_at IS NULL AND company_id = ",
        );
        query.push_bind(company_id);
        crate::sql::push_in(&mut query, "id", &customers);
        let found: i64 = query.build_query_scalar().fetch_one(pool).await?;
        if usize::try_from(found).ok() != Some(customers.len()) {
            return Err(bad_request("unknown_contact"));
        }
    }
    if !users.is_empty() {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT COUNT(*) FROM users WHERE is_deleted = 0 AND company_id = ",
        );
        query.push_bind(company_id);
        crate::sql::push_in(&mut query, "id", &users);
        let found: i64 = query.build_query_scalar().fetch_one(pool).await?;
        if usize::try_from(found).ok() != Some(users.len()) {
            return Err(bad_request("unknown_coworker"));
        }
    }
    Ok((customers, users))
}

/// Locks the company's groups for a write that checks names or counts.
async fn lock_company(tx: &mut Transaction<'_, MySql>, company_id: i32) -> sqlx::Result<()> {
    sqlx::query("SELECT id FROM company WHERE id = ? FOR UPDATE")
        .bind(company_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 400 `name_taken` when another group I see (the company's, or my own) has
/// this name.
async fn check_free_name(
    tx: &mut Transaction<'_, MySql>,
    company_id: i32,
    me: i32,
    name: &str,
    except: Option<i64>,
) -> ApiResult<()> {
    let taken: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM phone_contact_groups
          WHERE company_id = ? AND (shared <> 0 OR created_by = ?)
            AND LOWER(name) = LOWER(?) AND id <> ?",
    )
    .bind(company_id)
    .bind(me)
    .bind(name)
    .bind(except.unwrap_or(0))
    .fetch_one(&mut **tx)
    .await?;
    if taken > 0 {
        return Err(bad_request("name_taken"));
    }
    Ok(())
}

/// Makes the group's members exactly these, keeping who was already in it
/// (and who added them).
async fn write_members(
    tx: &mut Transaction<'_, MySql>,
    group_id: i64,
    by: i32,
    customers: &[i32],
    users: &[i32],
) -> sqlx::Result<()> {
    let current: Vec<MemberRow> = sqlx::query_as(
        "SELECT id, group_id, customer_id, user_id
           FROM phone_contact_group_members WHERE group_id = ?",
    )
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await?;
    let gone: Vec<i64> = current
        .iter()
        .filter(|member| match (member.customer_id, member.user_id) {
            (Some(customer), _) => customers.binary_search(&customer).is_err(),
            (None, Some(user)) => users.binary_search(&user).is_err(),
            (None, None) => true,
        })
        .map(|member| member.id)
        .collect();
    if !gone.is_empty() {
        let mut query =
            QueryBuilder::<MySql>::new("DELETE FROM phone_contact_group_members WHERE group_id = ");
        query.push_bind(group_id);
        crate::sql::push_in(&mut query, "id", &gone);
        query.build().execute(&mut **tx).await?;
    }
    let had_customer = |id: i32| current.iter().any(|m| m.customer_id == Some(id));
    let had_user = |id: i32| current.iter().any(|m| m.user_id == Some(id));
    let added: Vec<(Option<i32>, Option<i32>)> = customers
        .iter()
        .filter(|&&id| !had_customer(id))
        .map(|&id| (Some(id), None))
        .chain(
            users
                .iter()
                .filter(|&&id| !had_user(id))
                .map(|&id| (None, Some(id))),
        )
        .collect();
    if !added.is_empty() {
        let mut query = QueryBuilder::<MySql>::new(
            "INSERT IGNORE INTO phone_contact_group_members
               (group_id, customer_id, user_id, added_by) ",
        );
        query.push_values(added, |mut row, (customer, user)| {
            row.push_bind(group_id)
                .push_bind(customer)
                .push_bind(user)
                .push_bind(by);
        });
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

/// List contact groups
///
/// The company's contact groups (made by any of its users) with their
/// members, by name, and which of them I hid from the top of my Contacts;
/// also whether I hid the built-in Coworkers. Members are ids to show from
/// the contact index (`GET /v1/phone/contacts/index`: customers by `id`,
/// coworkers by `userId`); deleted customers and users are left out. Change
/// a group with `PATCH /v1/phone/contact-groups/{id}` and
/// `PUT /v1/phone/contact-groups/{id}/members`. Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/contact-groups",
    operation_id = "list_phone_contact_groups",
    tag = "phone",
    responses(
        (status = 200, description = "The company's groups and what I hid", body = ContactGroups),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn list_contact_groups(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<ContactGroups>> {
    let user = &device.user;
    let pool = &state.pool;
    let can_share = may_share(pool, user).await?;
    let groups = load(pool, user.company_id, user.id, can_share, None).await?;
    let coworkers_hidden: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM phone_contact_group_hidden
          WHERE user_id = ? AND builtin = 'coworkers'",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await?;
    Ok(Json(ContactGroups {
        coworkers_hidden: coworkers_hidden > 0,
        can_share,
        groups,
    }))
}

/// Create a contact group
///
/// A new group with me as its maker, and optionally its first members. Made
/// by a Manager, a Sales Manager, an admin or a superuser it is the
/// company's (`shared` true): every user of the company sees it at the top
/// of their Contacts (until they hide it). Made by anyone else it is their
/// own (`shared` false): only they see it. `GET /v1/phone/contact-groups`
/// `canShare` says which mine will be. At most 100 groups that I see (400
/// `too_many_groups`) and 500 members per group (400 `too_many_members`).
/// 400 `name_taken`, `unknown_contact`, `unknown_coworker`, or what is
/// wrong with the name. Device token only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/contact-groups",
    operation_id = "create_phone_contact_group",
    tag = "phone",
    request_body = NewGroup,
    responses(
        (status = 201, description = "The new group", body = ContactGroup),
        (status = 400, description = "`name_taken`, `too_many_groups`, `too_many_members`, `unknown_contact`, `unknown_coworker`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn create_contact_group(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<NewGroup>,
) -> ApiResult<(StatusCode, Json<ContactGroup>)> {
    let user = &device.user;
    let pool = &state.pool;
    let name = checked_name(&input.name)?;
    let (customers, users) =
        checked_members(pool, user.company_id, &input.customer_ids, &input.user_ids).await?;
    let mut tx = pool.begin().await?;
    lock_company(&mut tx, user.company_id).await?;
    let shared = may_share(pool, user).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM phone_contact_groups
          WHERE company_id = ? AND (shared <> 0 OR created_by = ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_GROUPS {
        return Err(bad_request("too_many_groups"));
    }
    check_free_name(&mut tx, user.company_id, user.id, &name, None).await?;
    let id = sqlx::query(
        "INSERT INTO phone_contact_groups (company_id, name, created_by, shared)
         VALUES (?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(user.id)
    .bind(shared)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i64::try_from(id).unwrap_or(i64::MAX);
    write_members(&mut tx, id, user.id, &customers, &users).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(one(pool, &device, id).await?)))
}

/// Rename, hide or show a contact group
///
/// `name` renames the group for everyone who sees it (400 as for
/// `POST /v1/phone/contact-groups`; 403 for the company's group unless I am
/// a Manager, a Sales Manager, an admin or a superuser: `canEdit`);
/// `hidden` hides it from the top of my own Contacts or shows it there
/// again (other users' tops are left alone; the group and its members
/// stay), which anyone may do. Send either or both. 404 when the group is
/// not one I see. Device token only.
#[crate::openapi::path(
    patch,
    path = "/v1/phone/contact-groups/{id}",
    operation_id = "update_phone_contact_group",
    tag = "phone",
    params(("id" = i64, Path, description = "The group's id")),
    request_body = GroupChange,
    responses(
        (status = 200, description = "The group now", body = ContactGroup),
        (status = 400, description = "`name_taken`, or what is wrong", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "Renaming the company's group without being a Manager, a Sales Manager, an admin or a superuser", body = ErrorBody),
        (status = 404, description = "Not a group I see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn update_contact_group(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<GroupChange>,
) -> ApiResult<Json<ContactGroup>> {
    let user = &device.user;
    let pool = &state.pool;
    let group = one(pool, &device, id).await?;
    if input.name.is_none() && input.hidden.is_none() {
        return Err(ApiError::BadRequest("Send a name or hidden.".into()));
    }
    if input.name.is_some() && !group.can_edit {
        return Err(ApiError::Forbidden);
    }
    if let Some(name) = input.name.as_deref() {
        let name = checked_name(name)?;
        let mut tx = pool.begin().await?;
        lock_company(&mut tx, user.company_id).await?;
        check_free_name(&mut tx, user.company_id, user.id, &name, Some(id)).await?;
        sqlx::query("UPDATE phone_contact_groups SET name = ? WHERE id = ? AND company_id = ?")
            .bind(&name)
            .bind(id)
            .bind(user.company_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    match input.hidden {
        Some(true) => {
            sqlx::query(
                "INSERT IGNORE INTO phone_contact_group_hidden (user_id, group_id) VALUES (?, ?)",
            )
            .bind(user.id)
            .bind(id)
            .execute(pool)
            .await?;
        }
        Some(false) => {
            sqlx::query(
                "DELETE FROM phone_contact_group_hidden WHERE user_id = ? AND group_id = ?",
            )
            .bind(user.id)
            .bind(id)
            .execute(pool)
            .await?;
        }
        None => {}
    }
    Ok(Json(one(pool, &device, id).await?))
}

/// Hide or show Coworkers
///
/// Hides the built-in Coworkers group (everyone at the company with a phone
/// number) from the top of my Contacts, or shows it there again. Only for
/// me; Coworkers itself is unchanged. `GET /v1/phone/contact-groups`
/// `coworkersHidden` says which. Device token only.
#[crate::openapi::path(
    patch,
    path = "/v1/phone/contact-groups/coworkers",
    operation_id = "update_phone_coworkers_group",
    tag = "phone",
    request_body = CoworkersChange,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No `hidden`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn update_coworkers_group(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<CoworkersChange>,
) -> ApiResult<StatusCode> {
    let statement = if input.hidden {
        "INSERT IGNORE INTO phone_contact_group_hidden (user_id, builtin) VALUES (?, 'coworkers')"
    } else {
        "DELETE FROM phone_contact_group_hidden WHERE user_id = ? AND builtin = 'coworkers'"
    };
    sqlx::query(statement)
        .bind(device.user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a contact group
///
/// Deletes the group for everyone who sees it (its customers and coworkers
/// themselves stay). To only take it off the top of my Contacts, hide it
/// instead (`PATCH /v1/phone/contact-groups/{id}` `hidden`). 403 for the
/// company's group unless I am a Manager, a Sales Manager, an admin or a
/// superuser (`canEdit`); 404 when the group is not one I see. Device token
/// only.
#[crate::openapi::path(
    delete,
    path = "/v1/phone/contact-groups/{id}",
    operation_id = "delete_phone_contact_group",
    tag = "phone",
    params(("id" = i64, Path, description = "The group's id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "The company's group, and I am not a Manager, a Sales Manager, an admin or a superuser", body = ErrorBody),
        (status = 404, description = "Not a group I see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn delete_contact_group(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    editable(&state.pool, &device, id).await?;
    let deleted = sqlx::query("DELETE FROM phone_contact_groups WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(device.user.company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Set a contact group's members
///
/// Makes the group's members exactly the customers and coworkers sent
/// (send the whole list: who is left out is taken out; empty lists empty
/// the group). For everyone who sees the group. At most 500 members (400
/// `too_many_members`); 400 `unknown_contact` / `unknown_coworker` for ids
/// that are not my company's live customers / users. 403 for the company's
/// group unless I am a Manager, a Sales Manager, an admin or a superuser
/// (`canEdit`); 404 when the group is not one I see. Device token only.
#[crate::openapi::path(
    put,
    path = "/v1/phone/contact-groups/{id}/members",
    operation_id = "set_phone_contact_group_members",
    tag = "phone",
    params(("id" = i64, Path, description = "The group's id")),
    request_body = GroupMembers,
    responses(
        (status = 200, description = "The group now", body = ContactGroup),
        (status = 400, description = "`too_many_members`, `unknown_contact` or `unknown_coworker`", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "The company's group, and I am not a Manager, a Sales Manager, an admin or a superuser", body = ErrorBody),
        (status = 404, description = "Not a group I see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn set_contact_group_members(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<GroupMembers>,
) -> ApiResult<Json<ContactGroup>> {
    let user = &device.user;
    let pool = &state.pool;
    editable(pool, &device, id).await?;
    let (customers, users) =
        checked_members(pool, user.company_id, &input.customer_ids, &input.user_ids).await?;
    let mut tx = pool.begin().await?;
    // The group's row orders two writes of its members.
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM phone_contact_groups WHERE id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound);
    }
    write_members(&mut tx, id, user.id, &customers, &users).await?;
    tx.commit().await?;
    Ok(Json(one(pool, &device, id).await?))
}
