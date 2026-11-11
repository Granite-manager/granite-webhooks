use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use sqlx::mysql::MySqlPoolOptions;
use std::time::Duration;

// Seed: in company 100 Rita (100) is a sales rep and owns the open deal 8002
// "Erin countertops" of Erin Builder (1004); Mark (101) is the Sales Manager;
// Ada (102) is the admin. "Project on a sale" starts off; `turn_on` turns it
// on.

async fn run(pool: &MySqlPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn turn_on(pool: &MySqlPool) {
    run(pool, "UPDATE company SET auto_project = 1 WHERE id = 100").await;
}

async fn win_8002(pool: &MySqlPool) {
    server(pool.clone())
        .put("/v1/deals/8002/outcome")
        .with_session(sessions::ADMIN)
        .json(&json!({ "outcome": "won" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

async fn projects_of_8002(pool: &MySqlPool) -> Vec<i32> {
    sqlx::query_scalar("SELECT id FROM projects WHERE source_deal_id = 8002")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn board(pool: &MySqlPool, session: &str) -> Value {
    let res = server(pool.clone())
        .get("/v1/projects")
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json()
}

fn ids(board: &Value) -> Vec<i64> {
    board["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect()
}

async fn act(pool: &MySqlPool, id: i32, session: &str, body: Value) -> axum_test::TestResponse {
    server(pool.clone())
        .post(&format!("/v1/projects/{id}/actions"))
        .with_session(session)
        .json(&body)
        .await
}

async fn version(pool: &MySqlPool, id: i32) -> i32 {
    sqlx::query_scalar("SELECT version FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn a_columns_manual_order_is_saved_for_production_only(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        &format!("UPDATE projects SET position = 5 WHERE id = {id}"),
    )
    .await;
    let reorder = |session: &'static str, ids: Value| {
        let pool = pool.clone();
        async move {
            server(pool)
                .post("/v1/projects/reorder")
                .with_session(session)
                .json(&json!({ "ids": ids }))
                .await
        }
    };
    let before = version(&pool, id).await;

    reorder(sessions::REP, json!([id]))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    reorder(sessions::ADMIN, json!([id, 999_999]))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    reorder(sessions::ADMIN, json!([]))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let position = || async {
        sqlx::query_scalar::<_, i32>("SELECT position FROM projects WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(position().await, 5);

    reorder(sessions::ADMIN, json!([id]))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(position().await, 0);
    assert_eq!(version(&pool, id).await, before);
}

#[test_db::test(seed)]
async fn projects_need_no_switch_and_a_win_adds_none_until_project_on_a_sale(pool: MySqlPool) {
    assert!(ids(&board(&pool, sessions::ADMIN).await).is_empty());
    win_8002(&pool).await;
    assert!(projects_of_8002(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn a_sold_deal_goes_to_projects(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let created = projects_of_8002(&pool).await;
    assert_eq!(created.len(), 1);
    let id = created[0];

    let admin = board(&pool, sessions::ADMIN).await;
    assert_eq!(ids(&admin), [i64::from(id)]);
    let project = &admin["projects"][0];
    assert_eq!(project["stage_code"], "new");
    assert_eq!(project["title"], "Erin countertops");
    assert_eq!(project["customer_name"], "Erin Builder");
    assert_eq!(project["sales_rep_id"], 100);
    assert_eq!(
        admin["stages"],
        json!(["new", "templated", "fabrication", "installed", "completed"])
    );

    // Winning it again starts nothing more; the deal page finds it.
    win_8002(&pool).await;
    assert_eq!(projects_of_8002(&pool).await, [id]);
    let deal = server(pool.clone())
        .get("/v1/deals/8002/project")
        .with_session(sessions::REP)
        .await;
    deal.assert_status_ok();
    assert_eq!(deal.json::<Value>()["project_id"], id);
}

#[test_db::test(seed)]
async fn each_position_sees_its_part_of_the_board(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let only = [i64::from(id)];

    // Rita sold it. Mark, the Sales Manager, sees every project, in the
    // employee app too.
    assert_eq!(ids(&board(&pool, sessions::REP).await), only);
    assert_eq!(ids(&board(&pool, sessions::MANAGER).await), only);
    let employee_side = server(pool.clone())
        .get("/v1/projects?employee=true")
        .with_session(sessions::MANAGER)
        .await
        .json::<Value>();
    assert_eq!(ids(&employee_side), only);
    assert_eq!(
        board(&pool, sessions::MANAGER).await["stages"],
        json!(["new", "templated", "fabrication", "installed", "completed"])
    );

    // Another rep's project is not Rita's.
    run(
        &pool,
        &format!("UPDATE projects SET sales_rep_id = 102 WHERE id = {id}"),
    )
    .await;
    assert!(ids(&board(&pool, sessions::REP).await).is_empty());
    server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // An Inventory Manager sees every project too.
    run(
        &pool,
        "UPDATE users_positions SET position_id = 12 WHERE user_id = 101 AND company_id = 100",
    )
    .await;
    assert_eq!(ids(&board(&pool, sessions::MANAGER).await), only);

    // A rep sees another rep's project once given a task, and keeps it when done.
    let created = server(pool.clone())
        .post(&format!("/v1/projects/{id}/tasks"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Make the layout", "assignedUserId": 100 }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let task = created.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(ids(&board(&pool, sessions::REP).await), only);
    server(pool.clone())
        .patch(&format!("/v1/projects/{id}/tasks/{task}"))
        .with_session(sessions::REP)
        .json(&json!({ "completed": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(ids(&board(&pool, sessions::REP).await), only);
    server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status_ok();

    // No position, no board.
    run(
        &pool,
        "DELETE FROM users_positions WHERE user_id = 101 AND company_id = 100",
    )
    .await;
    server(pool.clone())
        .get("/v1/projects")
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn without_project_on_a_sale_a_project_is_added_by_hand(pool: MySqlPool) {
    turn_on(&pool).await;
    run(&pool, "UPDATE company SET auto_project = 0 WHERE id = 100").await;
    win_8002(&pool).await;
    assert!(projects_of_8002(&pool).await.is_empty());

    // Rita adds it from her own deal; adding it again finds the same one.
    let server = server(pool.clone());
    let added = server
        .post("/v1/deals/8002/project")
        .with_session(sessions::REP)
        .await;
    added.assert_status_success();
    let id = projects_of_8002(&pool).await;
    assert_eq!(id.len(), 1);
    server
        .post("/v1/deals/8002/project")
        .with_session(sessions::REP)
        .await
        .assert_status_success();
    assert_eq!(projects_of_8002(&pool).await, id);
}

#[test_db::test(seed)]
async fn a_lost_deal_gets_no_project(pool: MySqlPool) {
    turn_on(&pool).await;
    run(&pool, "UPDATE deals SET is_won = 0 WHERE id = 8002").await;
    server(pool.clone())
        .post("/v1/deals/8002/project")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert!(projects_of_8002(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn a_task_tells_its_person_now_and_when_it_is_due(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let server = server(pool.clone());

    // Only people of the company.
    server
        .post(&format!("/v1/projects/{id}/tasks"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Expedite", "assignedUserId": 106 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Ada asks Mark to expedite the template; it was due long ago.
    let created = server
        .post(&format!("/v1/projects/{id}/tasks"))
        .with_session(sessions::ADMIN)
        .json(&json!({
            "name": "Expedite the template",
            "assignedUserId": 101,
            "deadline": "2020-01-01T10:00",
            "priority": "high",
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let task = created.json::<Value>()["id"].as_i64().unwrap();

    let bell = |session: &'static str| {
        let request = server.get("/v1/notifications").with_session(session);
        async move {
            let body = request.await.json::<Value>();
            body["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| n["message"] == "Expedite the template")
                .cloned()
                .collect::<Vec<Value>>()
        }
    };
    let marks = bell(sessions::MANAGER).await;
    let titles: Vec<&str> = marks
        .iter()
        .map(|n| n["type_title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Gave You a Task"), "{marks:?}");
    assert!(titles.contains(&"Task Due"), "{marks:?}");
    let href = format!("/employee/projects/edit/{id}");
    assert!(marks.iter().all(|n| n["href"] == href.as_str()));
    // Ada gave it; she gets nothing.
    assert!(bell(sessions::ADMIN).await.is_empty());

    // Done: the due reminder leaves the bell.
    server
        .patch(&format!("/v1/projects/{id}/tasks/{task}"))
        .with_session(sessions::MANAGER)
        .json(&json!({ "completed": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after = bell(sessions::MANAGER).await;
    assert!(
        after.iter().all(|n| n["type_title"] != "Task Due"),
        "{after:?}"
    );
    // Ada, who gave it, hears it is done.
    let adas = bell(sessions::ADMIN).await;
    assert!(
        adas.iter().any(|n| n["type_title"] == "Finished Your Task"),
        "{adas:?}"
    );

    // Only its maker (Ada) or a manager deletes it.
    server
        .delete(&format!("/v1/projects/{id}/tasks/{task}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn payment_is_confirmed_once_and_a_stale_page_is_told(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        "INSERT INTO sales (id, customer_id, seller_id, company_id, price, status, project_address, sale_date)
         VALUES (7100, 1004, 100, 100, 500.00, 'pending', '5 Oak Ave', NOW())",
    )
    .await;
    let stale = version(&pool, id).await;
    let linked = act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "link-sale", "version": stale, "saleId": 7100 }),
    )
    .await;
    linked.assert_status_ok();
    assert_eq!(linked.json::<Value>()["sale_id"], 7100);

    // The page still had the old version.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "paid", "version": stale, "date": "2026-01-05" }),
    )
    .await
    .assert_status(StatusCode::CONFLICT);

    let paid =
        json!({ "intent": "paid", "version": version(&pool, id).await, "date": "2026-01-05" });
    act(&pool, id, sessions::ADMIN, paid)
        .await
        .assert_status_ok();
    let again = act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "paid", "version": version(&pool, id).await, "date": "2026-01-06" }),
    )
    .await;
    again.assert_status(StatusCode::CONFLICT);
    let error = again.json::<Value>()["error"].as_str().unwrap().to_string();
    assert!(error.contains("already confirmed"), "{error}");

    // A sales rep doesn't move stages.
    act(
        &pool,
        id,
        sessions::REP,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "templated" }),
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn the_schedule_and_the_crew_move_a_project(pool: MySqlPool) {
    use chrono::Utc;
    use chrono_tz::America::Indiana::Indianapolis;

    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let stage = || {
        let pool = pool.clone();
        async move { board(&pool, sessions::ADMIN).await["projects"][0]["stage_code"].clone() }
    };
    assert_eq!(stage().await, "new");

    // Mark (101) makes templates. The sold quote's template waits unscheduled.
    let today = Utc::now().with_timezone(&Indianapolis).date_naive();
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9880, 100, 'Templates', 'blue', 'templates'),
           (9881, 100, 'Installations', 'red', 'installations');
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 11, 100);",
    )
    .await;
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id)
         VALUES (9870, 'Template', ?, ?, 1, 1, 102, 101, 9880, ?)",
    )
    .bind(today.and_hms_opt(0, 0, 0).unwrap())
    .bind(today.and_hms_opt(23, 59, 59).unwrap())
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(stage().await, "new");

    // The office puts it on the calendar: it stays, and its status says so.
    run(&pool, "UPDATE events SET unscheduled = 0 WHERE id = 9870").await;
    assert_eq!(stage().await, "new");
    let card = board(&pool, sessions::ADMIN).await["projects"][0].clone();
    assert!(card["template_date"].is_string(), "{card:?}");

    // Mark taps Completed: templated, with no one moving it.
    server(pool.clone())
        .post("/v1/events/9870/crew-status")
        .with_session(sessions::MANAGER)
        .json(&json!({ "status": "completed" }))
        .await
        .assert_status_ok();
    assert_eq!(stage().await, "templated");
    let page: Value = server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(
        page["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["source_action"] == "template-completed"),
        "{page:?}"
    );

    // The shop marks it fabricated: it waits in In Fabrication.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "fabricated", "version": version(&pool, id).await }),
    )
    .await
    .assert_status_ok();
    assert_eq!(stage().await, "fabrication");

    // The installation on the calendar, then the installer's Completed.
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id)
         VALUES (9871, 'Install', ?, ?, 1, 0, 102, 101, 9881, ?)",
    )
    .bind(today.and_hms_opt(0, 0, 0).unwrap())
    .bind(today.and_hms_opt(23, 59, 59).unwrap())
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(stage().await, "fabrication");
    let card = board(&pool, sessions::ADMIN).await["projects"][0].clone();
    assert!(card["install_date"].is_string(), "{card:?}");
    assert!(
        card["template_date"].is_null(),
        "the template is done: {card:?}"
    );
    server(pool.clone())
        .post("/v1/events/9871/crew-status")
        .with_session(sessions::MANAGER)
        .json(&json!({ "status": "completed" }))
        .await
        .assert_status_ok();
    assert_eq!(stage().await, "installed");

    // Anyone who moves projects can move it back by hand; it stays there.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "templated" }),
    )
    .await
    .assert_status_ok();
    assert_eq!(stage().await, "templated");
}

#[test_db::test(seed)]
async fn a_project_moved_back_stays_there_until_moved_forward(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let stage = || {
        let pool = pool.clone();
        async move { board(&pool, sessions::ADMIN).await["projects"][0]["stage_code"].clone() }
    };
    // The template maker tapped Completed: Templated.
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9880, 100, 'Templates', 'blue', 'templates');",
    )
    .await;
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id,
                             crew_status)
         VALUES (9870, 'Template', UTC_TIMESTAMP(), UTC_TIMESTAMP(), 1, 0, 102, 101, 9880, ?,
                 'completed')",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(stage().await, "templated");

    // Back to Awaiting Template: the completed template doesn't move it on.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "new" }),
    )
    .await
    .assert_status_ok();
    assert_eq!(stage().await, "new");

    // Forward by hand again: the calendar counts once more.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "templated" }),
    )
    .await
    .assert_status_ok();
    assert_eq!(stage().await, "templated");
}

#[test_db::test(seed)]
async fn the_page_shows_the_crews_job_photos(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9881, 100, 'Installations', 'red', 'installations');",
    )
    .await;
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id)
         VALUES (9871, 'Install', NOW(), NOW(), 1, 0, 102, 101, 9881, ?)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // A job photo, a comment picture only the team sees, and a deleted photo.
    run(
        &pool,
        "INSERT INTO event_images (event_id, company_id, url, created_by, for_customer, deleted_at) VALUES
           (9871, 100, 'https://cdn.example/job.jpg', 101, 1, NULL),
           (9871, 100, 'https://cdn.example/comment.jpg', 101, 0, NULL),
           (9871, 100, 'https://cdn.example/gone.jpg', 101, 1, NOW());",
    )
    .await;
    let page: Value = server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    let photos = page["job_photos"].as_array().unwrap();
    assert_eq!(photos.len(), 1, "{photos:?}");
    assert_eq!(photos[0]["url"], "https://cdn.example/job.jpg");
    assert_eq!(photos[0]["event_id"], 9871);
    assert_eq!(photos[0]["kind"], "installations");
    assert_eq!(photos[0]["author"], "Mark Manager");
}

// The company's own process steps.

async fn add_step(
    pool: &MySqlPool,
    session: &str,
    stage: &str,
    name: &str,
) -> axum_test::TestResponse {
    server(pool.clone())
        .post("/v1/projects/process-steps")
        .with_session(session)
        .json(&json!({ "stage": stage, "name": name }))
        .await
}

#[test_db::test(seed)]
async fn admins_set_up_process_steps_and_projects_check_them_off(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let project = projects_of_8002(&pool).await[0];

    add_step(&pool, sessions::REP, "new", "Confirm a file")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    add_step(&pool, sessions::ADMIN, "completed", "Too late")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    add_step(&pool, sessions::ADMIN, "new", "   ")
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let first = add_step(&pool, sessions::ADMIN, "new", "Confirm a file").await;
    first.assert_status(StatusCode::CREATED);
    let first = first.json::<Value>()["id"].as_i64().unwrap();
    let second = add_step(&pool, sessions::ADMIN, "new", "Call the customer").await;
    let second = second.json::<Value>()["id"].as_i64().unwrap();

    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["process_steps"][0]["name"], "Confirm a file");
    assert_eq!(seen["done_steps"], json!([]));

    server(pool.clone())
        .post("/v1/projects/process-steps/reorder")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [second, first] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["process_steps"][0]["name"], "Call the customer");

    server(pool.clone())
        .patch(&format!("/v1/projects/process-steps/{first}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Confirm the file" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let check = async |done: bool| {
        server(pool.clone())
            .put(&format!("/v1/projects/{project}/steps/{first}"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "done": done }))
            .await
    };
    check(true).await.assert_status(StatusCode::NO_CONTENT);
    check(true).await.assert_status(StatusCode::NO_CONTENT);
    let page = server(pool.clone())
        .get(&format!("/v1/projects/{project}"))
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(page["done_steps"].as_array().unwrap().len(), 1);
    assert_eq!(page["done_steps"][0]["step_id"].as_i64(), Some(first));

    server(pool.clone())
        .delete(&format!("/v1/projects/process-steps/{}", first + 9999))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server(pool.clone())
        .delete(&format!("/v1/projects/process-steps/{first}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_process_steps_done")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[test_db::test(seed)]
async fn checking_a_step_off_tells_its_people(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let project = projects_of_8002(&pool).await[0];
    let step = add_step(&pool, sessions::ADMIN, "new", "Send the file").await;
    let step = step.json::<Value>()["id"].as_i64().unwrap();
    let notify = async |session: &str, people: Value| {
        server(pool.clone())
            .put(&format!("/v1/projects/process-steps/{step}/notify"))
            .with_session(session)
            .json(&json!({ "user_ids": people }))
            .await
    };
    notify(sessions::REP, json!([100]))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    notify(sessions::ADMIN, json!([100, 999_999]))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    notify(sessions::ADMIN, json!([101, 100, 102, 100]))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let seen = board(&pool, sessions::ADMIN).await;
    let mut people: Vec<i64> = seen["process_steps"][0]["notify"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_i64().unwrap())
        .collect();
    people.sort_unstable();
    assert_eq!(people, vec![100, 101, 102]);

    // Ada (102) checks it off: Rita and Mark hear, Ada doesn't; twice tells once.
    for _ in 0..2 {
        server(pool.clone())
            .put(&format!("/v1/projects/{project}/steps/{step}"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "done": true }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let told: Vec<(i32, Option<i32>, String)> = sqlx::query_as(
        "SELECT user_id, project_id, message FROM notifications
          WHERE notification_type = 'project_step_done' ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        told,
        vec![
            (100, Some(project), "Send the file".to_string()),
            (101, Some(project), "Send the file".to_string()),
        ]
    );

    // The phone reads it to open the project.
    let rita: u64 = sqlx::query_scalar(
        "SELECT id FROM notifications
          WHERE notification_type = 'project_step_done' AND user_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let item = server(pool.clone())
        .get(&format!("/v1/notifications/{rita}"))
        .with_session(sessions::REP)
        .await;
    item.assert_status_ok();
    let item = item.json::<Value>();
    assert_eq!(item["project_id"].as_i64(), Some(i64::from(project)));
    assert_eq!(item["type_title"], "Finished a Step");
    assert_eq!(item["message"], "Send the file");

    notify(sessions::ADMIN, json!([]))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["process_steps"][0]["notify"], json!([]));
}

#[test_db::test(seed)]
async fn steps_check_themselves_off_by_their_rules(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let project = projects_of_8002(&pool).await[0];
    let id = async |name: &str| {
        add_step(&pool, sessions::ADMIN, "new", name)
            .await
            .json::<Value>()["id"]
            .as_i64()
            .unwrap()
    };
    let file = id("Get the file").await;
    let wait = id("Wait two days").await;
    let auto = async |step: i64, session, rule: Value| {
        server(pool.clone())
            .put(&format!("/v1/projects/process-steps/{step}/auto"))
            .with_session(session)
            .json(&json!({ "auto": rule }))
            .await
    };
    auto(file, sessions::REP, json!({ "trigger": "file_added" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    auto(file, sessions::ADMIN, json!({ "trigger": "nothing" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    auto(file, sessions::ADMIN, json!({ "trigger": "task_done" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    auto(
        file,
        sessions::ADMIN,
        json!({ "trigger": "email_template", "ref_id": 999_999 }),
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);
    auto(file, sessions::ADMIN, json!({ "trigger": "file_added" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    auto(
        wait,
        sessions::ADMIN,
        json!({ "trigger": "days_after", "days": 2 }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["process_steps"][0]["auto"]["trigger"], "file_added");
    assert_eq!(seen["process_steps"][1]["auto"]["days"], 2);
    assert_eq!(seen["done_steps"], json!([]));

    // A file comes in: the first step checks off; the second waits two days.
    sqlx::query(
        "INSERT INTO project_files (project_id, company_id, name, url, content_type, created_by, created_at)
         SELECT p.id, p.company_id, 'layout.pdf', 'x/layout.pdf', 'application/pdf',
                (SELECT u.id FROM users u WHERE u.company_id = p.company_id LIMIT 1),
                UTC_TIMESTAMP() + INTERVAL 1 MINUTE
           FROM projects p WHERE p.id = ?",
    )
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    let seen = board(&pool, sessions::ADMIN).await;
    let done = seen["done_steps"].as_array().unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["step_id"].as_i64(), Some(file));
    assert_eq!(done[0]["done_by"], "Auto");

    sqlx::query(
        "UPDATE project_process_steps_done SET done_at = UTC_TIMESTAMP(3) - INTERVAL 3 DAY",
    )
    .execute(&pool)
    .await
    .unwrap();
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["done_steps"].as_array().unwrap().len(), 2);

    // Unchecked by hand, it stays unchecked.
    server(pool.clone())
        .put(&format!("/v1/projects/{project}/steps/{wait}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "done": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["done_steps"].as_array().unwrap().len(), 1);

    auto(file, sessions::ADMIN, Value::Null)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(seen["process_steps"][0]["auto"], Value::Null);
}

/// The Lambda's pool has one connection: the board and a project's page must
/// not check the rules, or tell a step's people, while they hold it (each
/// load waited out the acquire timeout and checked nothing).
#[test_db::test(seed)]
async fn rules_are_checked_on_a_one_connection_pool(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let project = projects_of_8002(&pool).await[0];
    let step = add_step(&pool, sessions::ADMIN, "new", "Get the file").await;
    let step = step.json::<Value>()["id"].as_i64().unwrap();
    for (path, body) in [
        ("auto", json!({ "auto": { "trigger": "file_added" } })),
        ("notify", json!({ "user_ids": [101] })),
    ] {
        server(pool.clone())
            .put(&format!("/v1/projects/process-steps/{step}/{path}"))
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    sqlx::query(
        "INSERT INTO project_files (project_id, company_id, name, url, content_type, created_by, created_at)
         SELECT p.id, p.company_id, 'layout.pdf', 'x/layout.pdf', 'application/pdf',
                (SELECT u.id FROM users u WHERE u.company_id = p.company_id LIMIT 1),
                UTC_TIMESTAMP() + INTERVAL 1 MINUTE
           FROM projects p WHERE p.id = ?",
    )
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    let single = MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let told = async || -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE project_id = ? AND user_id = 101",
        )
        .bind(project)
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    let seen = board(&single, sessions::ADMIN).await;
    assert_eq!(seen["done_steps"].as_array().unwrap().len(), 1);
    assert_eq!(told().await, 1);

    run(&pool, "DELETE FROM project_process_steps_done").await;
    let page = server(single.clone())
        .get(&format!("/v1/projects/{project}"))
        .with_session(sessions::ADMIN)
        .await;
    page.assert_status_ok();
    assert_eq!(
        page.json::<Value>()["done_steps"].as_array().unwrap().len(),
        1
    );
    assert_eq!(told().await, 2);
}

#[test_db::test(seed)]
async fn the_page_and_board_show_the_crews_issues(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9881, 100, 'Installations', 'red', 'installations');",
    )
    .await;
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id)
         VALUES (9871, 'Install', NOW(), NOW(), 1, 0, 102, 101, 9881, ?)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // Two issues, one with pictures, and a deleted one.
    run(
        &pool,
        "INSERT INTO event_issues (id, event_id, company_id, description, created_by, created_by_name, deleted_at) VALUES
           (9901, 9871, 100, 'Sink cutout too small', 101, 'Alex', NULL),
           (9902, 9871, 100, 'Seam is off', 101, 'Alex', NULL),
           (9903, 9871, 100, 'Gone', 101, 'Alex', NOW());
         INSERT INTO event_issue_images (issue_id, url) VALUES
           (9901, 'https://cdn.example/a.jpg'), (9901, 'https://cdn.example/b.jpg');",
    )
    .await;
    let page: Value = server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    let issues = page["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "{issues:?}");
    let sink = issues.iter().find(|issue| issue["id"] == 9901).unwrap();
    assert_eq!(sink["description"], "Sink cutout too small");
    assert_eq!(sink["kind"], "installations");
    assert_eq!(sink["author"], "Alex");
    assert_eq!(
        sink["photos"],
        json!(["https://cdn.example/a.jpg", "https://cdn.example/b.jpg"])
    );
    let seen = board(&pool, sessions::ADMIN).await;
    assert_eq!(
        seen["issue_counts"],
        json!([{ "project_id": id, "count": 2 }])
    );
}

#[test_db::test(seed)]
async fn issues_are_reported_on_a_project_and_marked_solved(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9881, 100, 'Installations', 'red', 'installations');",
    )
    .await;
    sqlx::query(
        "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                             created_user_id, assigned_user_id, calendar_id, project_id)
         VALUES (9871, 'Install', NOW(), NOW(), 1, 0, 102, 101, 9881, ?)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    run(
        &pool,
        "INSERT INTO event_issues (id, event_id, company_id, description, created_by, created_by_name) VALUES
           (9901, 9871, 100, 'Sink cutout too small', 101, 'Alex');",
    )
    .await;

    let report = async |text: &str| {
        server(pool.clone())
            .post(&format!("/v1/projects/{id}/issues"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "description": text }))
            .await
    };
    report("   ").await.assert_status(StatusCode::BAD_REQUEST);
    report("Customer wants an ogee edge")
        .await
        .assert_status(StatusCode::CREATED);
    let page = async || {
        server(pool.clone())
            .get(&format!("/v1/projects/{id}"))
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>()
    };
    let seen = page().await;
    let issues = seen["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "{issues:?}");
    let own = issues
        .iter()
        .find(|issue| issue["event_id"].is_null())
        .unwrap();
    assert_eq!(own["description"], "Customer wants an ogee edge");
    let own_id = own["id"].as_i64().unwrap();
    assert_eq!(
        board(&pool, sessions::ADMIN).await["issue_counts"][0]["count"],
        2
    );

    // The crew's issue, solved from the project; the project's, from... the project.
    let solve = async |issue: i64, solved: bool| {
        server(pool.clone())
            .put(&format!("/v1/projects/{id}/issues/{issue}"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "solved": solved }))
            .await
    };
    solve(9901, true)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    solve(own_id + 9999, true)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let seen = page().await;
    let crew = seen["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["id"] == 9901)
        .unwrap()
        .clone();
    assert!(crew["solved_at"].is_string());
    assert!(crew["solved_by"].is_string());
    // Open issues come first.
    assert_eq!(seen["issues"][0]["id"].as_i64(), Some(own_id));
    assert_eq!(
        board(&pool, sessions::ADMIN).await["issue_counts"][0]["count"],
        1
    );

    // On the job card, the crew opens it again.
    server(pool.clone())
        .put("/v1/events/9871/issues/9901")
        .with_session(sessions::ADMIN)
        .json(&json!({ "solved": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let details: Value = server(pool.clone())
        .get("/v1/events/9871/details")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(details["issues"][0]["solved_at"].is_null(), "{details}");
    assert_eq!(
        board(&pool, sessions::ADMIN).await["issue_counts"][0]["count"],
        2
    );

    server(pool.clone())
        .delete(&format!("/v1/projects/{id}/issues/{own_id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // A job's issue is deleted on the job, not here.
    server(pool.clone())
        .delete(&format!("/v1/projects/{id}/issues/9901"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(page().await["issues"].as_array().unwrap().len(), 1);
}

// Parts: a project made of several, each with its own template and installation.

#[test_db::test(seed)]
async fn a_project_with_parts_sits_with_its_least_finished_part(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let stage = || {
        let pool = pool.clone();
        async move { board(&pool, sessions::ADMIN).await["projects"][0]["stage_code"].clone() }
    };
    let page = || {
        let pool = pool.clone();
        async move {
            let page: Value = server(pool)
                .get(&format!("/v1/projects/{id}"))
                .with_session(sessions::ADMIN)
                .await
                .json();
            page
        }
    };
    let job = |event: i32, calendar: i32, part: Option<i64>| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO events (id, title, start_date, end_date, all_day, unscheduled,
                                     created_user_id, assigned_user_id, calendar_id, project_id,
                                     project_part_id, crew_status)
                 VALUES (?, 'Job', UTC_TIMESTAMP(), UTC_TIMESTAMP(), 1, 0, 102, 101, ?, ?, ?,
                         'completed')",
            )
            .bind(event)
            .bind(calendar)
            .bind(id)
            .bind(part)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    run(
        &pool,
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9880, 100, 'Templates', 'blue', 'templates'),
           (9881, 100, 'Installations', 'red', 'installations');",
    )
    .await;
    // The countertops are templated before the project has any part.
    job(9870, 9880, None).await;
    assert_eq!(stage().await, "templated");
    assert_eq!(page().await["parts"], json!([]));

    // A company starts with three part types; Cabinets skip template and fabrication.
    let types = page().await["part_types"].clone();
    let names: Vec<&str> = types
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Countertops", "Backsplash", "Cabinets"]);
    assert_eq!(types[2]["uses_template"], false);
    assert_eq!(types[2]["uses_fabrication"], false);
    let add = |body: Value| {
        let pool = pool.clone();
        async move {
            let res = server(pool)
                .post(&format!("/v1/projects/{id}/parts"))
                .with_session(sessions::ADMIN)
                .json(&body)
                .await;
            res.assert_status(StatusCode::CREATED);
            res.json::<Value>()["id"].as_i64().unwrap()
        }
    };

    // A step done before the project had parts stays with its first part.
    let step = add_step(&pool, sessions::ADMIN, "templated", "Confirm the file")
        .await
        .json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let check = |body: Value| {
        let pool = pool.clone();
        async move {
            server(pool)
                .put(&format!("/v1/projects/{id}/steps/{step}"))
                .with_session(sessions::ADMIN)
                .json(&body)
                .await
                .assert_status(StatusCode::NO_CONTENT);
        }
    };
    check(json!({ "done": true })).await;

    // Adding a backsplash: the work so far becomes the first part, where the
    // project was, and the project goes back to Awaiting Template.
    let backsplash = add(json!({ "type_id": types[1]["id"] })).await;
    let parts = page().await["parts"].clone();
    assert_eq!(parts.as_array().unwrap().len(), 2, "{parts:?}");
    assert_eq!(parts[0]["name"], "Countertops");
    assert_eq!(parts[0]["stage_code"], "templated");
    assert_eq!(parts[1]["name"], "Backsplash");
    assert_eq!(parts[1]["stage_code"], "new");
    assert_eq!(stage().await, "new");
    let countertops = parts[0]["id"].as_i64().unwrap();
    assert_eq!(page().await["done_steps"][0]["part_id"], countertops);
    assert_eq!(
        board(&pool, sessions::ADMIN).await["parts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Cabinets only wait for their installation, so they hold nothing back.
    let cabinets = add(json!({ "type_id": types[2]["id"], "name": "Kitchen cabinets" })).await;
    let parts = page().await["parts"].clone();
    assert_eq!(parts[2]["name"], "Kitchen cabinets");
    assert_eq!(parts[2]["stage_code"], "fabrication");
    assert_eq!(stage().await, "new");
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "new",
                "partId": cabinets }),
    )
    .await
    .assert_status(StatusCode::CONFLICT);

    // The backsplash's own template moves only the backsplash.
    // Booked for no part: it goes to the part still waiting for a template.
    job(9871, 9880, None).await;
    let parts = page().await["parts"].clone();
    assert_eq!(parts[1]["stage_code"], "templated");
    assert_eq!(stage().await, "templated");
    // A step is checked off for one part: without a part, the least finished.
    check(json!({ "done": true, "part_id": backsplash })).await;
    let done = page().await["done_steps"].clone();
    assert_eq!(done.as_array().unwrap().len(), 2, "{done:?}");
    check(json!({ "done": false, "part_id": backsplash })).await;
    assert_eq!(page().await["done_steps"].as_array().unwrap().len(), 1);

    // Moved back by hand, one part: its finished template no longer counts.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "new",
                "partId": backsplash }),
    )
    .await
    .assert_status_ok();
    assert_eq!(page().await["parts"][1]["stage_code"], "new");
    assert_eq!(page().await["parts"][0]["stage_code"], "templated");
    assert_eq!(stage().await, "new");
    let history = page().await["history"].clone();
    assert!(
        history
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["source_action"] == "moved-back" && line["part_name"] == "Backsplash"),
        "{history:?}"
    );

    // The cabinets are installed: the project still waits for the other parts.
    job(9872, 9881, Some(cabinets)).await;
    assert_eq!(page().await["parts"][2]["stage_code"], "installed");
    assert_eq!(stage().await, "new");

    // A part with jobs stays; its job can be moved to another part first.
    let remove = |part: i64| {
        let pool = pool.clone();
        async move {
            server(pool)
                .delete(&format!("/v1/projects/{id}/parts/{part}"))
                .with_session(sessions::ADMIN)
                .await
                .status_code()
        }
    };
    assert_eq!(remove(backsplash).await, StatusCode::CONFLICT);
    server(pool.clone())
        .put(&format!("/v1/projects/{id}/appointments/9871/part"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "part_id": countertops }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(remove(backsplash).await, StatusCode::NO_CONTENT);
    assert_eq!(page().await["parts"].as_array().unwrap().len(), 2);
    assert_eq!(stage().await, "templated");

    // A sales rep sees the part types but only admins change them.
    server(pool.clone())
        .get("/v1/projects/part-types")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    server(pool.clone())
        .post("/v1/projects/part-types")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Tile", "uses_template": false, "uses_fabrication": false }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let tile = server(pool.clone())
        .post("/v1/projects/part-types")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Tile", "uses_template": true, "uses_fabrication": false }))
        .await;
    tile.assert_status(StatusCode::CREATED);
    let tile = tile.json::<Value>()["id"].as_i64().unwrap();
    server(pool.clone())
        .patch(&format!("/v1/projects/part-types/{tile}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Tile work", "uses_template": false, "uses_fabrication": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server(pool.clone())
        .delete(&format!("/v1/projects/part-types/{tile}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let left: Value = server(pool.clone())
        .get("/v1/projects/part-types")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(left.as_array().unwrap().len(), 3);
}
