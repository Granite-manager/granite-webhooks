use crate::common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use axum::extract::RawQuery;
use axum::http::{StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Drive minutes between the company (H) and the stops: C is farthest from
/// H, then C→A and A→B are short.
fn minutes(from: &str, to: &str) -> Option<u32> {
    let rank = |place: &str| {
        ["Home", "Stop A", "Stop B", "Stop C", "Nowhere"]
            .iter()
            .position(|p| *p == place)
    };
    match (rank(from)?, rank(to)?) {
        (_, 4) | (4, _) => None,
        (0, 3) => Some(40),
        (3, 1) | (1, 2) => Some(5),
        _ => Some(30),
    }
}

async fn google() -> String {
    let router = Router::new()
        .route(
            "/matrix",
            post(|Json(body): Json<Value>| async move {
                // Best days sends points, not addresses: no route for those.
                let name = |point: &Value| point["waypoint"]["address"].as_str().unwrap_or_default().to_string();
                let origins: Vec<String> = body["origins"].as_array().unwrap().iter().map(name).collect();
                let destinations: Vec<String> = body["destinations"].as_array().unwrap().iter().map(name).collect();
                let mut elements = Vec::new();
                for (o, from) in origins.iter().enumerate() {
                    for (d, to) in destinations.iter().enumerate() {
                        elements.push(match minutes(from, to) {
                            Some(m) => json!({ "originIndex": o, "destinationIndex": d, "condition": "ROUTE_EXISTS", "duration": format!("{}s", m * 60) }),
                            None => json!({ "originIndex": o, "destinationIndex": d, "condition": "ROUTE_NOT_FOUND" }),
                        });
                    }
                }
                Json(Value::Array(elements))
            }),
        )
        .route(
            "/geocode/json",
            // Every address at 39.9, but `Near` 0.1° north, `Far` 1° north
            // and `Lost` nowhere.
            get(|RawQuery(query): RawQuery| async move {
                let address = query
                    .unwrap_or_default()
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("address=").map(str::to_string))
                    .unwrap_or_default();
                let lat = match address.as_str() {
                    "Lost" => return Json(json!({ "status": "ZERO_RESULTS", "results": [] })),
                    "Near" => 40.0,
                    "Far" => 40.9,
                    _ => 39.9,
                };
                Json(json!({ "status": "OK", "results": [{ "geometry": { "location": { "lat": lat, "lng": -86.1 } } }] }))
            }),
        )
        .route(
            "/routes",
            post(|| async { Json(json!({ "routes": [{ "polyline": { "encodedPolyline": "_p~iF~ps|U" } }] })) }),
        )
        .route(
            "/staticmap",
            // An image only when the line follows the roads.
            get(|RawQuery(query): RawQuery| async move {
                assert!(query.unwrap_or_default().contains("enc%3A%5Fp%7EiF%7Eps%7CU"));
                ([(header::CONTENT_TYPE, "image/png")], vec![137u8, 80, 78, 71])
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

async fn server(pool: MySqlPool) -> TestServer {
    let base = google().await;
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_maps(
        Maps::new(Some("key"), &format!("{base}/matrix"), &base)
            .with_routes_url(&format!("{base}/routes")),
    );
    TestServer::new(app(state))
}

const DAY: &str = "
UPDATE company SET address = 'Home' WHERE id = 100;
INSERT INTO events (id, title, location, start_date, end_date, all_day, created_user_id) VALUES
  (9100, 'A', 'Stop A', '2030-01-15 14:00:00', '2030-01-15 15:00:00', 0, 100),
  (9101, 'B', 'Stop B', '2030-01-15 15:00:00', '2030-01-15 15:30:00', 0, 100),
  (9102, 'C', 'Stop C', '2030-01-15 16:00:00', '2030-01-15 18:00:00', 0, 100),
  (9103, 'Lost', 'Nowhere', '2030-01-15 17:00:00', '2030-01-15 18:00:00', 0, 100);
";

#[test_db::test(seed)]
async fn a_route_reorders_the_day(pool: MySqlPool) {
    sqlx::raw_sql(DAY).execute(&pool).await.unwrap();
    let server = server(pool.clone()).await;
    let res = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9101, 9102] }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "order": [9102, 9100, 9101], "changed": 3, "stops": 3 })
    );
    let times: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT id, CAST(start_date AS CHAR), CAST(end_date AS CHAR) FROM events WHERE id IN (9100, 9101, 9102) ORDER BY start_date",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        times,
        [
            (
                9102,
                "2030-01-15 14:00:00".into(),
                "2030-01-15 16:00:00".into()
            ),
            (
                9100,
                "2030-01-15 15:00:00".into(),
                "2030-01-15 16:00:00".into()
            ),
            (
                9101,
                "2030-01-15 16:00:00".into(),
                "2030-01-15 16:30:00".into()
            ),
        ]
    );

    let unreachable = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9103] }))
        .await;
    unreachable.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        unreachable.json::<Value>()["error"],
        "Google Maps could not find a driving route to: Nowhere"
    );
    server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_route_preview_saves_nothing_until_approved(pool: MySqlPool) {
    sqlx::raw_sql(DAY).execute(&pool).await.unwrap();
    let server = server(pool.clone()).await;
    let times = || async {
        sqlx::query_as::<_, (i32, String)>(
            "SELECT id, CAST(start_date AS CHAR) FROM events WHERE id IN (9100, 9101, 9102) ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let before = times().await;
    let preview = server
        .post("/v1/schedule/route-preview")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9101, 9102] }))
        .await;
    preview.assert_status_ok();
    assert_eq!(
        preview.json::<Value>(),
        json!({
            "order": [9102, 9100, 9101],
            "moves": [
                { "id": 9102, "start": "2030-01-15T14:00:00Z", "end": "2030-01-15T16:00:00Z" },
                { "id": 9100, "start": "2030-01-15T15:00:00Z", "end": "2030-01-15T16:00:00Z" },
                { "id": 9101, "start": "2030-01-15T16:00:00Z", "end": "2030-01-15T16:30:00Z" },
            ],
            "stops": 3,
        })
    );
    assert_eq!(times().await, before);

    // Approving saves exactly the given order.
    let approved = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9101, 9102], "order": [9101, 9100, 9102] }))
        .await;
    approved.assert_status_ok();
    assert_eq!(
        approved.json::<Value>(),
        json!({ "order": [9101, 9100, 9102], "changed": 2, "stops": 3 })
    );
    assert_eq!(
        times().await,
        [
            (9100, "2030-01-15 15:00:00".into()),
            (9101, "2030-01-15 14:00:00".into()),
            (9102, "2030-01-15 16:00:00".into()),
        ]
    );

    let mismatch = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9101, 9102], "order": [9100, 9101] }))
        .await;
    mismatch.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        mismatch.json::<Value>()["error"],
        "The route's stops changed. Build the route again."
    );
}

#[test_db::test(seed)]
async fn a_day_map_is_drawn(pool: MySqlPool) {
    sqlx::raw_sql(DAY).execute(&pool).await.unwrap();
    let body: Value = server(pool)
        .await
        .get("/v1/schedule/day-map?address=Stop%20A&address=Stop%20B")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["home_address"], "Home");
    assert_eq!(body["image"], "data:image/png;base64,iVBORw==");
    assert_eq!(body["home"], json!({ "lat": 39.9, "lng": -86.1 }));
    assert_eq!(body["stops"].as_array().unwrap().len(), 2);
    assert_eq!(body["route"], "_p~iF~ps|U");
}

#[test_db::test(seed)]
async fn best_days_measure_the_later_stops_of_a_jobs_calendar(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9150, 100, 'Templates', 'red', 'templates'),
           (9151, 100, 'Installations', 'blue', 'installations');
         INSERT INTO events (id, title, location, start_date, end_date, all_day, created_user_id, calendar_id, unscheduled, rrule) VALUES
           (9160, 'Waiting', 'Job', '2030-01-01 12:00:00', '2030-01-01 12:00:00', 1, 100, 9150, 1, NULL),
           (9161, 'Same place', 'job', '2030-01-15 14:00:00', '2030-01-15 15:00:00', 0, 100, 9150, 0, NULL),
           (9162, 'Near', 'Near', '2030-01-16 14:00:00', '2030-01-16 15:00:00', 0, 100, 9150, 0, NULL),
           (9163, 'Far', 'Far', '2030-01-17 14:00:00', '2030-01-17 15:00:00', 0, 100, 9150, 0, NULL),
           (9164, 'Lost', 'Lost', '2030-01-17 16:00:00', '2030-01-17 17:00:00', 0, 100, 9150, 0, NULL),
           (9165, 'Other calendar', 'Near', '2030-01-16 14:00:00', '2030-01-16 15:00:00', 0, 100, 9151, 0, NULL),
           (9166, 'No address', NULL, '2030-01-16 14:00:00', '2030-01-16 15:00:00', 0, 100, 9150, 0, NULL),
           (9167, 'Past', 'Near', '2020-01-16 14:00:00', '2020-01-16 15:00:00', 0, 100, 9150, 0, NULL),
           (9168, 'Repeats', 'Near', '2030-01-16 14:00:00', '2030-01-16 15:00:00', 0, 100, 9150, 0, 'FREQ=WEEKLY'),
           (9169, 'Also waiting', 'Near', '2030-01-16 14:00:00', '2030-01-16 15:00:00', 0, 100, 9150, 1, NULL),
           (9170, 'After the range', 'Near', '2030-03-16 14:00:00', '2030-03-16 15:00:00', 0, 100, 9150, 0, NULL),
           (9171, 'Waiting install', 'Job', '2030-01-01 12:00:00', '2030-01-01 12:00:00', 1, 100, 9151, 1, NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone()).await;
    let range =
        json!({ "event_id": 9160, "start": "2020-01-01T00:00:00Z", "end": "2030-02-01T00:00:00Z" });
    let body: Value = server
        .post("/v1/schedule/best-days")
        .with_session(sessions::ADMIN)
        .json(&range)
        .await
        .json();
    // Only the later one-off stops of the job's calendar that Google finds.
    assert_eq!(
        body,
        json!({ "stops": [
            { "event_id": 9161, "miles": 0.0, "minutes": null },
            { "event_id": 9162, "miles": 6.9, "minutes": null },
            { "event_id": 9163, "miles": 69.1, "minutes": null },
        ] })
    );
    // A waiting installation measures the stops of its own calendar.
    let install: Value = server
        .post("/v1/schedule/best-days")
        .with_session(sessions::ADMIN)
        .json(&json!({ "event_id": 9171, "start": "2020-01-01T00:00:00Z", "end": "2030-02-01T00:00:00Z" }))
        .await
        .json();
    assert_eq!(
        install,
        json!({ "stops": [{ "event_id": 9165, "miles": 6.9, "minutes": null }] })
    );
    server
        .post("/v1/schedule/best-days")
        .with_session(sessions::ADMIN)
        .json(&json!({ "event_id": 9999, "start": "2020-01-01T00:00:00Z", "end": "2030-02-01T00:00:00Z" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // A job Google cannot find has no best days.
    sqlx::query("UPDATE events SET location = 'Lost' WHERE id = 9160")
        .execute(&pool)
        .await
        .unwrap();
    let lost: Value = server
        .post("/v1/schedule/best-days")
        .with_session(sessions::ADMIN)
        .json(&range)
        .await
        .json();
    assert_eq!(lost, json!({ "stops": [] }));
}
