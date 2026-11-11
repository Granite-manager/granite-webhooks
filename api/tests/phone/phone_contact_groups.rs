//! Contact groups in the app's Contacts (`routes/phone/contact_groups.rs`):
//! the company's groups (made by managers, sales managers and admins,
//! shared by its users), each other user's own groups, and each user's own
//! hidden list (Coworkers included).
//!
//! Fixture phones use n = 401..=410.

use crate::phone_common;

use axum::http::{Method, StatusCode};
use phone_common::{
    HEADER, add_session, assert_device_only, phone_server, revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita (100) and Mark (101, a Sales Manager) of company 100, Olga (106) of
/// company 101; Vera (104, made an employee by the test) is of company 100
/// and holds no position.
const RITA: u32 = 401;
const MARK: u32 = 402;
const OLGA: u32 = 403;
const VERA: u32 = 405;

/// The sessions, with Rita a Manager: she and Mark make the company's groups.
async fn sessions(pool: &MySqlPool) {
    sql(
        pool,
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 5, 100)",
    )
    .await;
    add_session(pool, RITA, 100, 100).await;
    add_session(pool, MARK, 101, 100).await;
    add_session(pool, OLGA, 106, 101).await;
}

async fn list(server: &axum_test::TestServer, n: u32) -> Value {
    let res = server
        .get("/v1/phone/contact-groups")
        .add_header(HEADER, token(n))
        .await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn the_routes_take_only_the_device_token(pool: MySqlPool) {
    add_session(&pool, 404, 100, 100).await;
    revoke_session(&pool, 404).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        404,
        &[
            (Method::GET, "/v1/phone/contact-groups".into()),
            (Method::POST, "/v1/phone/contact-groups".into()),
            (Method::PATCH, "/v1/phone/contact-groups/1".into()),
            (Method::DELETE, "/v1/phone/contact-groups/1".into()),
            (Method::PUT, "/v1/phone/contact-groups/1/members".into()),
            (Method::PATCH, "/v1/phone/contact-groups/coworkers".into()),
        ],
    )
    .await;
}

#[test_db::test(seed)]
#[allow(clippy::too_many_lines)]
async fn groups_are_the_companys_and_hiding_is_mine(pool: MySqlPool) {
    sessions(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_eq!(
        list(&server, RITA).await,
        json!({ "coworkersHidden": false, "canShare": true, "groups": [] })
    );

    // Rita makes a group with customers and a coworker; repeats count once.
    let res = server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(RITA))
        .json(
            &json!({ "name": "  Installers ", "customerIds": [1004, 1000, 1004],
                       "userIds": [101] }),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    let group: Value = res.json();
    let id = group["id"].as_i64().unwrap();
    let installers = json!({ "id": id, "name": "Installers", "hidden": false,
                             "customerIds": [1000, 1004], "userIds": [101], "createdBy": 100,
                             "shared": true, "canEdit": true });
    assert_eq!(group, installers);
    let (made_by,): (Option<i32>,) =
        sqlx::query_as("SELECT created_by FROM phone_contact_groups WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(made_by, Some(100));

    // Mark (same company) sees it; Olga (another company) does not.
    assert_eq!(
        list(&server, MARK).await,
        json!({ "coworkersHidden": false, "canShare": true, "groups": [installers] })
    );
    assert_eq!(list(&server, OLGA).await["groups"], json!([]));

    // Mark hides it and Coworkers: only from his own top.
    let res = server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["hidden"], true);
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Twice is the same.
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let marks = list(&server, MARK).await;
    assert_eq!(marks["coworkersHidden"], true);
    assert_eq!(marks["groups"][0]["hidden"], true);
    assert_eq!(marks["groups"][0]["customerIds"], json!([1000, 1004]));
    assert_eq!(
        list(&server, RITA).await,
        json!({ "coworkersHidden": false, "canShare": true, "groups": [installers] })
    );

    // Mark renames it for everyone; the name is checked.
    let res = server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .json(&json!({ "name": "Crew" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["name"], "Crew");
    assert_eq!(list(&server, RITA).await["groups"][0]["name"], "Crew");
    for (body, error) in [
        (json!({ "name": "crew " }), Some("name_taken")),
        (json!({ "name": " " }), None),
        (json!({ "name": "x".repeat(61) }), None),
    ] {
        let res = server
            .post("/v1/phone/contact-groups")
            .add_header(HEADER, token(RITA))
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        if let Some(error) = error {
            assert_eq!(res.json::<Value>()["error"], error, "{body}");
        }
    }
    // Its own name again (another case) is fine; nothing to change is not.
    server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(RITA))
        .json(&json!({ "name": "CREW" }))
        .await
        .assert_status_ok();
    server
        .patch(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(RITA))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Members: exactly what is sent, checked against the company.
    let res = server
        .put(&format!("/v1/phone/contact-groups/{id}/members"))
        .add_header(HEADER, token(RITA))
        .json(&json!({ "customerIds": [1010, 1004], "userIds": [100, 101] }))
        .await;
    res.assert_status_ok();
    let group: Value = res.json();
    assert_eq!(group["customerIds"], json!([1004, 1010]));
    assert_eq!(group["userIds"], json!([100, 101]));
    // Who stayed keeps who added them.
    let (added_by,): (Option<i32>,) = sqlx::query_as(
        "SELECT added_by FROM phone_contact_group_members WHERE group_id = ? AND customer_id = 1004",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(added_by, Some(100));
    for (body, error) in [
        (json!({ "customerIds": [2000] }), "unknown_contact"),
        (json!({ "customerIds": [1003] }), "unknown_contact"),
        (json!({ "userIds": [106] }), "unknown_coworker"),
        (json!({ "userIds": [105] }), "unknown_coworker"),
        (
            json!({ "customerIds": (1..=501).collect::<Vec<i32>>() }),
            "too_many_members",
        ),
    ] {
        let res = server
            .put(&format!("/v1/phone/contact-groups/{id}/members"))
            .add_header(HEADER, token(MARK))
            .json(&body)
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], error, "{body}");
    }
    // A customer deleted later is left out.
    sql(
        &pool,
        "UPDATE customers SET deleted_at = UTC_TIMESTAMP() WHERE id = 1010",
    )
    .await;
    assert_eq!(
        list(&server, MARK).await["groups"][0]["customerIds"],
        json!([1004])
    );

    // Another company's user can't touch it.
    for request in [
        server
            .patch(&format!("/v1/phone/contact-groups/{id}"))
            .json(&json!({ "name": "Mine" })),
        server
            .put(&format!("/v1/phone/contact-groups/{id}/members"))
            .json(&json!({ "customerIds": [] })),
        server
            .delete(&format!("/v1/phone/contact-groups/{id}"))
            .json(&json!({})),
    ] {
        request
            .add_header(HEADER, token(OLGA))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // Mark deletes it for everyone (with his hidden mark); Coworkers stays hidden for him.
    server
        .delete(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(list(&server, RITA).await["groups"], json!([]));
    assert_eq!(
        list(&server, MARK).await,
        json!({ "coworkersHidden": true, "canShare": true, "groups": [] })
    );
    let (left,): (i64,) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM phone_contact_group_members WHERE group_id = ?)
              + (SELECT COUNT(*) FROM phone_contact_group_hidden WHERE group_id = ?)",
    )
    .bind(id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
    server
        .delete(&format!("/v1/phone/contact-groups/{id}"))
        .add_header(HEADER, token(MARK))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Shown again.
    server
        .patch("/v1/phone/contact-groups/coworkers")
        .add_header(HEADER, token(MARK))
        .json(&json!({ "hidden": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(list(&server, MARK).await["coworkersHidden"], false);
}

#[test_db::test(seed)]
async fn groups_are_listed_by_name_and_limited(pool: MySqlPool) {
    sessions(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    for name in ["builders", "Architects", "Crew"] {
        server
            .post("/v1/phone/contact-groups")
            .add_header(HEADER, token(RITA))
            .json(&json!({ "name": name }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let names: Vec<Value> = list(&server, MARK).await["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["name"].clone())
        .collect();
    assert_eq!(
        names,
        [json!("Architects"), json!("builders"), json!("Crew")]
    );
    // Another company may use the same name.
    server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(OLGA))
        .json(&json!({ "name": "Crew" }))
        .await
        .assert_status(StatusCode::CREATED);
    // At most 100 groups that a user sees.
    sql(
        &pool,
        "INSERT INTO phone_contact_groups (company_id, name)
         WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 97)
         SELECT 100, CONCAT('Group ', i) FROM n",
    )
    .await;
    let res = server
        .post("/v1/phone/contact-groups")
        .add_header(HEADER, token(RITA))
        .json(&json!({ "name": "One more" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "too_many_groups");
}

#[test_db::test(seed)]
async fn a_group_is_the_companys_only_when_a_manager_makes_it(pool: MySqlPool) {
    sessions(&pool).await;
    sql(&pool, "UPDATE users SET is_employee = 1 WHERE id = 104").await;
    add_session(&pool, VERA, 104, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_eq!(list(&server, VERA).await["canShare"], false);
    assert_eq!(list(&server, MARK).await["canShare"], true);
    let make = |n: u32, name: &'static str| {
        server
            .post("/v1/phone/contact-groups")
            .add_header(HEADER, token(n))
            .json(&json!({ "name": name, "customerIds": [1000] }))
    };
    let names = |listed: Value| -> Vec<String> {
        listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["name"].as_str().unwrap().to_string())
            .collect()
    };

    // Vera's group is her own: only she sees it.
    let res = make(VERA, "My builders").await;
    res.assert_status(StatusCode::CREATED);
    let own: Value = res.json();
    assert_eq!(
        (&own["shared"], &own["canEdit"]),
        (&json!(false), &json!(true))
    );
    let own_id = own["id"].as_i64().unwrap();
    assert_eq!(names(list(&server, VERA).await), ["My builders"]);
    assert!(names(list(&server, MARK).await).is_empty());
    assert!(names(list(&server, RITA).await).is_empty());

    // Mark's (a Sales Manager) is the company's: everyone sees it, Vera can't change it.
    let res = make(MARK, "Designers").await;
    res.assert_status(StatusCode::CREATED);
    let shared: Value = res.json();
    assert_eq!(shared["shared"], true);
    let shared_id = shared["id"].as_i64().unwrap();
    let veras = list(&server, VERA).await;
    assert_eq!(names(veras.clone()), ["Designers", "My builders"]);
    assert_eq!(veras["groups"][0]["canEdit"], false);
    assert_eq!(list(&server, RITA).await["groups"][0]["canEdit"], true);
    for request in [
        server
            .patch(&format!("/v1/phone/contact-groups/{shared_id}"))
            .json(&json!({ "name": "Mine now" })),
        server
            .put(&format!("/v1/phone/contact-groups/{shared_id}/members"))
            .json(&json!({ "customerIds": [] })),
        server
            .delete(&format!("/v1/phone/contact-groups/{shared_id}"))
            .json(&json!({})),
    ] {
        request
            .add_header(HEADER, token(VERA))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    // She may still hide it from her own top.
    server
        .patch(&format!("/v1/phone/contact-groups/{shared_id}"))
        .add_header(HEADER, token(VERA))
        .json(&json!({ "hidden": true }))
        .await
        .assert_status_ok();

    // Nobody else reaches Vera's own group, a manager included.
    for request in [
        server
            .patch(&format!("/v1/phone/contact-groups/{own_id}"))
            .json(&json!({ "name": "Taken over" })),
        server
            .put(&format!("/v1/phone/contact-groups/{own_id}/members"))
            .json(&json!({ "customerIds": [] })),
        server
            .delete(&format!("/v1/phone/contact-groups/{own_id}"))
            .json(&json!({})),
    ] {
        request
            .add_header(HEADER, token(MARK))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    // Names: hers may not repeat the company's or her own; a manager's may repeat hers.
    for name in ["designers", "my builders"] {
        let res = make(VERA, name).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], "name_taken");
    }
    make(RITA, "My builders")
        .await
        .assert_status(StatusCode::CREATED);
    // She changes and deletes her own.
    server
        .put(&format!("/v1/phone/contact-groups/{own_id}/members"))
        .add_header(HEADER, token(VERA))
        .json(&json!({ "customerIds": [1004] }))
        .await
        .assert_status_ok();
    server
        .delete(&format!("/v1/phone/contact-groups/{own_id}"))
        .add_header(HEADER, token(VERA))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
