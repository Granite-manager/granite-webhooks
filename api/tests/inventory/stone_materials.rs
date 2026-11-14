use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Admin › Stones › Settings "Stone view" and the materials it shows: the
// stones of one name grouped, each stone one bundle with its block number.

fn stone(name: &str, bundle: &str, block: Option<&str>) -> Value {
    json!({
        "name": name,
        "type": "Granite",
        "finishing": "polished",
        "is_display": true,
        "on_sale": false,
        "regular_stock": false,
        "supplier_id": 301,
        "length": 120,
        "width": 70,
        "cost_per_sqft": 45,
        "retail_price": 99,
        "level": 3,
        "bundle_number": bundle,
        "block_number": block,
        "color_ids": []
    })
}

async fn add_stone(server: &TestServer, body: Value) -> i64 {
    let res = server
        .post("/v1/stones")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

async fn materials(server: &TestServer) -> Vec<Value> {
    let res = server
        .get("/v1/stones/materials?include_sold_out=true&include_hidden=true")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    res.json::<Vec<Value>>()
}

fn named<'a>(list: &'a [Value], name: &str) -> &'a Value {
    list.iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("no material {name} in {list:?}"))
}

#[test_db::test(seed)]
async fn the_old_stones_view_is_the_default(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .get("/v1/stones/settings")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "stone_view": "stones" }));
}

#[test_db::test(seed)]
async fn only_admins_change_the_view(pool: MySqlPool) {
    let server = server(pool);
    server
        .put("/v1/stones/settings")
        .with_session(sessions::REP)
        .json(&json!({ "stone_view": "materials" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/stones/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "stone_view": "tiles" }))
        .await
        .assert_status_bad_request();
    server
        .put("/v1/stones/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "stone_view": "materials" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let view = server
        .get("/v1/stones/settings")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(view["stone_view"], "materials");
    // Back to the old view.
    server
        .put("/v1/stones/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "stone_view": "stones" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let view = server
        .get("/v1/stones/settings")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(view["stone_view"], "stones");
    // Another company keeps its own.
    let other = server
        .get("/v1/stones/settings")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json::<Value>();
    assert_eq!(other["stone_view"], "stones");
}

#[test_db::test(seed)]
async fn existing_stones_are_linked_to_the_material_of_their_name(pool: MySqlPool) {
    // Stones that exist before the switch, written straight to the table
    // (no material yet), with names differing in case and spaces.
    for (name, bundle) in [("Fantasy Brown", "FB-1"), ("fantasy brown ", "FB-2")] {
        sqlx::query(
            "INSERT INTO stones (name, type, company_id, is_display, bundle_number)
             VALUES (?, 'Granite', 100, 1, ?)",
        )
        .bind(name)
        .bind(bundle)
        .execute(&pool)
        .await
        .unwrap();
    }
    let unlinked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM stones WHERE company_id = 100 AND material_id IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(unlinked >= 2);

    let server = server(pool.clone());
    server
        .put("/v1/stones/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "stone_view": "materials" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Every named live stone of the company now has a material.
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM stones WHERE company_id = 100 AND deleted_at IS NULL
           AND material_id IS NULL AND TRIM(COALESCE(name, '')) <> ''",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);

    let list = materials(&server).await;
    let fantasy = named(&list, "Fantasy Brown");
    assert_eq!(fantasy["stones"], 2);
    // Other companies' stones are never linked to company 100's materials.
    let foreign: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM stones s JOIN stone_materials m ON m.id = s.material_id
         WHERE s.company_id <> m.company_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(foreign, 0);

    // Switching back keeps the links; the stones list is unchanged.
    server
        .put("/v1/stones/settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "stone_view": "stones" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let stones = server
        .get("/v1/stones?include_sold_out=true&include_hidden=true")
        .with_session(sessions::ADMIN)
        .await
        .json::<Vec<Value>>();
    assert_eq!(
        stones.iter().filter(|s| s["name"].as_str().unwrap_or("").trim().eq_ignore_ascii_case("fantasy brown")).count(),
        2
    );
}

#[test_db::test(seed)]
async fn a_material_lists_its_bundles_with_block_numbers(pool: MySqlPool) {
    let server = server(pool.clone());
    let first = add_stone(&server, stone("Cosmic Test Stone", "TM-2", Some("B-9"))).await;
    let second = add_stone(&server, stone("Cosmic Test Stone", "TM-1", None)).await;
    add_stone(&server, stone("Galaxy Test Stone", "BG-1", Some("B-1"))).await;

    let list = materials(&server).await;
    let taj = named(&list, "Cosmic Test Stone");
    assert_eq!(taj["stones"], 2);
    assert_eq!(taj["type"], "Granite");
    assert_eq!(named(&list, "Galaxy Test Stone")["stones"], 1);

    let id = taj["id"].as_i64().unwrap();
    let res = server
        .get(&format!("/v1/stones/materials/{id}"))
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    let detail = res.json::<Value>();
    assert_eq!(detail["name"], "Cosmic Test Stone");
    let stones = detail["stones"].as_array().unwrap();
    // By bundle number.
    assert_eq!(stones[0]["id"].as_i64(), Some(second));
    assert_eq!(stones[0]["bundle_number"], "TM-1");
    assert_eq!(stones[0]["block_number"], Value::Null);
    assert_eq!(stones[1]["id"].as_i64(), Some(first));
    assert_eq!(stones[1]["block_number"], "B-9");
    assert_eq!(stones[1]["material_id"].as_i64(), Some(id));

    // The stone itself carries the block number.
    let one = server
        .get(&format!("/v1/stones/{first}"))
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(one["block_number"], "B-9");

    // Another company cannot open it.
    server
        .get(&format!("/v1/stones/materials/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn renaming_a_stone_moves_it_to_the_material_of_its_new_name(pool: MySqlPool) {
    let server = server(pool.clone());
    let id = add_stone(&server, stone("Blue Pearl", "BP-1", None)).await;
    add_stone(&server, stone("Blue Pearl", "BP-2", None)).await;

    // Same name in other case: stays.
    server
        .put(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .json(&stone("blue pearl", "BP-1", Some("B-3")))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(named(&materials(&server).await, "Blue Pearl")["stones"], 2);

    server
        .put(&format!("/v1/stones/{id}"))
        .with_session(sessions::ADMIN)
        .json(&stone("Blue Pearl Royal", "BP-1", Some("B-3")))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = materials(&server).await;
    assert_eq!(named(&list, "Blue Pearl")["stones"], 1);
    assert_eq!(named(&list, "Blue Pearl Royal")["stones"], 1);
}

#[test_db::test(seed)]
async fn hidden_stones_need_an_admin(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/stones/materials?include_hidden=true")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/stones/materials")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}
