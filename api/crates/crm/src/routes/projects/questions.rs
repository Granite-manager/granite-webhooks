//! Questions on a project (owner, 2026-11-16): each one is its own small
//! chat with only the people asked. Only they are told (bell and phone);
//! anyone else who can open the project can read it and join, and is never
//! told. "Everyone" asks every person involved in the project.

use super::access::Viewer;
use super::schemas::Project;
use super::service::visible_project;
use super::tasks::clip;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlConnection};

/// The bell: someone asked you a question on a project.
pub const QUESTION_ASKED: &str = "project_question";
/// The bell: someone answered in a question you are in.
pub const QUESTION_REPLY: &str = "project_question_reply";

const MESSAGE_MAX: usize = 5000;

/// A question on a project, for its list.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct QuestionSummary {
    /// Its id.
    pub id: i32,
    /// Its first words.
    pub title: String,
    /// Asked to everyone involved in the project.
    pub everyone: bool,
    /// Who asked; null once that user is gone.
    pub created_by: Option<i32>,
    /// Their name.
    pub created_by_name: Option<String>,
    /// When it was asked, UTC ISO.
    pub created_at: String,
    /// When its last message was written, UTC ISO.
    pub last_message_at: String,
    /// When it was marked answered, UTC ISO; null while open.
    pub resolved_at: Option<String>,
    /// How many messages it has.
    pub message_count: i64,
    /// Messages the caller has not read (always 0 for someone not in it).
    pub unread: i64,
    /// The caller is in it (asked, or asked to, or joined).
    pub member: bool,
    /// The people in it (user ids), the asker first.
    #[sqlx(skip)]
    pub members: Vec<i32>,
}

/// One message of a question.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct QuestionMessage {
    /// Its id.
    pub id: i32,
    /// Who wrote it; null once that user is gone.
    pub user_id: Option<i32>,
    /// Their name.
    pub user_name: Option<String>,
    /// The text.
    pub body: String,
    /// When, UTC ISO.
    pub created_at: String,
}

/// `GET /v1/projects/{id}/questions/{question_id}`: a question and its messages.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuestionThread {
    /// The question.
    pub question: QuestionSummary,
    /// Its messages, oldest first.
    pub messages: Vec<QuestionMessage>,
}

/// Body of `POST /v1/projects/{id}/questions`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionInput {
    /// The question: 1 to 5000 characters.
    pub text: String,
    /// Who it is for (user ids in the company); ignored with `everyone`.
    #[serde(default)]
    pub to: Vec<i32>,
    /// Ask everyone involved in the project.
    #[serde(default)]
    pub everyone: bool,
}

/// Body of `POST /v1/projects/{id}/questions/{question_id}/messages`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionReply {
    /// 1 to 5000 characters.
    pub text: String,
}

/// Body of `PUT /v1/projects/{id}/questions/{question_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionChange {
    /// People to add (they are told).
    #[serde(default)]
    pub add: Vec<i32>,
    /// The caller joins it (no one is told).
    #[serde(default)]
    pub join: bool,
    /// Answered (true) or open again (false); left out keeps it.
    #[serde(default)]
    pub resolved: Option<bool>,
}

fn text(raw: &str) -> ApiResult<String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest("Write the question".into()));
    }
    if text.chars().count() > MESSAGE_MAX {
        return Err(ApiError::BadRequest(
            "A message can be 5000 characters at most".into(),
        ));
    }
    Ok(text.to_string())
}

const SUMMARY_SELECT: &str = "SELECT q.id, q.title, q.everyone = 1 AS everyone, q.created_by,
        u.name AS created_by_name,
        DATE_FORMAT(q.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
        DATE_FORMAT(q.last_message_at, '%Y-%m-%dT%H:%i:%sZ') AS last_message_at,
        DATE_FORMAT(q.resolved_at, '%Y-%m-%dT%H:%i:%sZ') AS resolved_at,
        (SELECT COUNT(*) FROM project_question_messages qm WHERE qm.question_id = q.id) AS message_count,
        CAST(IF(me.user_id IS NULL, 0,
           (SELECT COUNT(*) FROM project_question_messages qm
             WHERE qm.question_id = q.id AND COALESCE(qm.user_id, 0) <> ?
               AND (me.last_read_at IS NULL OR qm.created_at > me.last_read_at))) AS SIGNED) AS unread,
        me.user_id IS NOT NULL AS member
   FROM project_questions q
   LEFT JOIN users u ON u.id = q.created_by
   LEFT JOIN project_question_members me ON me.question_id = q.id AND me.user_id = ?";

async fn fill_members(
    conn: &mut MySqlConnection,
    questions: &mut [QuestionSummary],
) -> sqlx::Result<()> {
    if questions.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = questions.iter().map(|q| q.id).collect();
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT m.question_id, m.user_id FROM project_question_members m
           JOIN project_questions q ON q.id = m.question_id WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "m.question_id", &ids);
    sql.push(" ORDER BY m.user_id = q.created_by DESC, m.user_id");
    let rows: Vec<(i32, i32)> = sql.build_query_as().fetch_all(&mut *conn).await?;
    for (question_id, user_id) in rows {
        if let Some(question) = questions.iter_mut().find(|q| q.id == question_id) {
            question.members.push(user_id);
        }
    }
    Ok(())
}

/// A project's questions for the caller: theirs first (unread first), then
/// the rest, newest first.
pub(super) async fn project_questions(
    conn: &mut MySqlConnection,
    project_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<QuestionSummary>> {
    let sql = format!(
        "{SUMMARY_SELECT} WHERE q.project_id = ?
          ORDER BY member DESC, q.resolved_at IS NOT NULL, q.last_message_at DESC, q.id DESC
          LIMIT 200"
    );
    let mut questions: Vec<QuestionSummary> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user_id)
        .bind(user_id)
        .bind(project_id)
        .fetch_all(&mut *conn)
        .await?;
    fill_members(conn, &mut questions).await?;
    Ok(questions)
}

async fn one_question(
    conn: &mut MySqlConnection,
    project_id: i32,
    question_id: i32,
    user_id: i32,
) -> ApiResult<QuestionSummary> {
    let sql = format!("{SUMMARY_SELECT} WHERE q.project_id = ? AND q.id = ?");
    let question: Option<QuestionSummary> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user_id)
        .bind(user_id)
        .bind(project_id)
        .bind(question_id)
        .fetch_optional(&mut *conn)
        .await?;
    let mut question = vec![question.ok_or(ApiError::NotFound)?];
    fill_members(conn, &mut question).await?;
    Ok(question.remove(0))
}

/// Only people in the company (check-in kiosks aside), without repeats.
async fn colleagues(
    conn: &mut MySqlConnection,
    company_id: i32,
    ids: &[i32],
) -> ApiResult<Vec<i32>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT id FROM users WHERE is_deleted = 0 AND company_id = ",
    );
    sql.push_bind(company_id);
    crate::sql::push_in(&mut sql, "id", ids);
    let found: Vec<i32> = sql.build_query_scalar().fetch_all(&mut *conn).await?;
    let mut unique: Vec<i32> = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if found.len() != unique.len() {
        return Err(ApiError::BadRequest("Pick people in your company".into()));
    }
    Ok(unique)
}

async fn add_members(
    conn: &mut MySqlConnection,
    question_id: i32,
    ids: &[i32],
) -> sqlx::Result<Vec<i32>> {
    let mut added = Vec::new();
    for id in ids {
        let rows = sqlx::query(
            "INSERT IGNORE INTO project_question_members (question_id, user_id) VALUES (?, ?)",
        )
        .bind(question_id)
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if rows > 0 {
            added.push(*id);
        }
    }
    Ok(added)
}

/// Tells these people (not the caller): bell and phone. Logged, never
/// returned: the message is saved.
async fn tell(
    state: &AppState,
    user: &SessionUser,
    project: &Project,
    kind: &str,
    title: &str,
    people: &[i32],
) {
    for person in people.iter().filter(|person| **person != user.id) {
        let result: sqlx::Result<()> = async {
            let id = sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, customer_id, message, notification_type, actor_name, due_at)
                 VALUES (?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(person)
            .bind(project.id)
            .bind(project.customer_id)
            .bind(clip(title, 255))
            .bind(kind)
            .bind(user.name.as_deref())
            .execute(&state.pool)
            .await?
            .last_insert_id();
            push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(
                ?error,
                project_id = project.id,
                "question notification failed"
            );
        }
    }
}

async fn add_message(
    conn: &mut MySqlConnection,
    question_id: i32,
    user: &SessionUser,
    body: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO project_question_messages (question_id, user_id, user_name, body)
         VALUES (?, ?, ?, ?)",
    )
    .bind(question_id)
    .bind(user.id)
    .bind(user.name.as_deref())
    .bind(body)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE project_questions SET last_message_at = UTC_TIMESTAMP(3) WHERE id = ?")
        .bind(question_id)
        .execute(&mut *conn)
        .await?;
    mark_read(conn, question_id, user.id).await
}

async fn mark_read(conn: &mut MySqlConnection, question_id: i32, user_id: i32) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE project_question_members SET last_read_at = UTC_TIMESTAMP(3)
          WHERE question_id = ? AND user_id = ?",
    )
    .bind(question_id)
    .bind(user_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Ask a question
///
/// Anyone who can open the project. Only the people in `to` (or, with
/// `everyone`, everyone involved in the project) are in it and told.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/questions",
    operation_id = "ask_project_question",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = QuestionInput,
    responses(
        (status = 201, description = "Asked", body = CreatedId),
        (status = 400, description = "Empty or long text, nobody picked, or someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn ask(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuestionInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let body = text(&input.text)?;
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    let people = if input.everyone {
        let steps = super::process::company_steps(&mut conn, viewer.company_id).await?;
        let owners = super::lists::step_owners(&mut conn, &project, &steps).await?;
        super::lists::involved(&mut conn, &project, &owners).await?
    } else {
        let to: Vec<i32> = input
            .to
            .iter()
            .copied()
            .filter(|id| *id != user.id)
            .collect();
        if to.is_empty() {
            return Err(ApiError::BadRequest("Pick who the question is for".into()));
        }
        colleagues(&mut conn, viewer.company_id, &to).await?
    };
    let title = clip(body.lines().next().unwrap_or_default(), 200);
    let mut tx = state.pool.begin().await?;
    let question = sqlx::query(
        "INSERT INTO project_questions (company_id, project_id, title, everyone, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(viewer.company_id)
    .bind(id)
    .bind(&title)
    .bind(input.everyone)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let question_id =
        i32::try_from(question).map_err(|_| ApiError::Internal("question id".into()))?;
    let mut members = vec![user.id];
    members.extend(people.iter().copied());
    add_members(&mut tx, question_id, &members).await?;
    add_message(&mut tx, question_id, &user, &body).await?;
    tx.commit().await?;
    drop(conn);
    tell(&state, &user, &project, QUESTION_ASKED, &title, &people).await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::from(question_id),
        }),
    ))
}

/// A question and its messages
///
/// Anyone who can open the project. Reading it marks it read for someone in it.
#[crate::openapi::path(
    get,
    path = "/v1/projects/{id}/questions/{question_id}",
    operation_id = "get_project_question",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("question_id" = i32, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The question", body = QuestionThread),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such question the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, question_id)): Path<(i32, i32)>,
) -> ApiResult<Json<QuestionThread>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let question = one_question(&mut conn, id, question_id, user.id).await?;
    let messages: Vec<QuestionMessage> = sqlx::query_as(
        "SELECT m.id, m.user_id, COALESCE(u.name, m.user_name) AS user_name, m.body,
                DATE_FORMAT(m.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_question_messages m LEFT JOIN users u ON u.id = m.user_id
          WHERE m.question_id = ? ORDER BY m.id",
    )
    .bind(question_id)
    .fetch_all(&mut *conn)
    .await?;
    if question.member {
        mark_read(&mut conn, question_id, user.id).await?;
    }
    Ok(Json(QuestionThread { question, messages }))
}

/// Answer in a question
///
/// Anyone who can open the project; someone not in it joins it. The others
/// in it are told.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/questions/{question_id}/messages",
    operation_id = "reply_project_question",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("question_id" = i32, Path, description = "Question id"),
    ),
    request_body = QuestionReply,
    responses(
        (status = 204, description = "Sent"),
        (status = 400, description = "Empty or long text", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such question the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reply(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, question_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<QuestionReply>,
) -> ApiResult<StatusCode> {
    let body = text(&input.text)?;
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    let question = one_question(&mut conn, id, question_id, user.id).await?;
    let mut tx = state.pool.begin().await?;
    add_members(&mut tx, question_id, &[user.id]).await?;
    add_message(&mut tx, question_id, &user, &body).await?;
    tx.commit().await?;
    drop(conn);
    tell(
        &state,
        &user,
        &project,
        QUESTION_REPLY,
        &question.title,
        &question.members,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Add people to a question, join it, or mark it answered
///
/// Anyone who can open the project. People added are told; joining tells nobody.
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/questions/{question_id}",
    operation_id = "change_project_question",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("question_id" = i32, Path, description = "Question id"),
    ),
    request_body = QuestionChange,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such question the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn change(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, question_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<QuestionChange>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    let question = one_question(&mut conn, id, question_id, user.id).await?;
    let add = colleagues(&mut conn, viewer.company_id, &input.add).await?;
    let mut joining = add.clone();
    if input.join {
        joining.push(user.id);
    }
    let added = add_members(&mut conn, question_id, &joining).await?;
    if input.join {
        mark_read(&mut conn, question_id, user.id).await?;
    }
    if let Some(resolved) = input.resolved {
        sqlx::query(
            "UPDATE project_questions SET resolved_at = IF(?, COALESCE(resolved_at, UTC_TIMESTAMP(3)), NULL)
              WHERE id = ?",
        )
        .bind(resolved)
        .bind(question_id)
        .execute(&mut *conn)
        .await?;
    }
    drop(conn);
    let told: Vec<i32> = added
        .into_iter()
        .filter(|person| add.contains(person))
        .collect();
    tell(
        &state,
        &user,
        &project,
        QUESTION_ASKED,
        &question.title,
        &told,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
