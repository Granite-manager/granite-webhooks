//! `POST /v1/ai/stone-colors`: a stone's colors picked by AI from its photo.
//!
//! The OpenAI Decisions API (`POST /v1/decisions`, `gpt-6-luna`) answers
//! which listed color (`GET /v1/colors`) is the stone's main one and, for
//! each color, how likely the stone shows it. The Add Stone and Edit Stone
//! forms fill the Color field with the pick when a photo is chosen; the
//! admin can change it before saving.

use crate::ai::{Caller, DECISIONS_MODEL};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// The largest photo read; the web app sends it shrunk to about 1 MB.
pub const MAX_PHOTO_BYTES: usize = 4 * 1024 * 1024;
/// The request body: the photo in base64 (4/3 of it) and the JSON around it.
pub const MAX_PHOTO_BODY_BYTES: usize = MAX_PHOTO_BYTES / 3 * 4 + 64 * 1024;
/// Most colors picked for one stone.
const MAX_PICKED: usize = 4;
/// A color besides the main one is picked when the AI is at least this sure
/// the stone shows it.
const SHOWS_COLOR_PROBABILITY: f64 = 0.5;
/// Name of the question about the stone's main color.
const MAIN_QUESTION: &str = "main";

const CONTEXT: &str = "This is a photo of a countertop stone slab (granite, quartz, marble, quartzite...) from a stone yard's inventory. Customers filter the inventory by color. Judge the stone itself only: ignore the background, floor, rack, lighting glare, shadows, labels and people.";

/// Body of `POST /v1/ai/stone-colors`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoneColorsInput {
    /// The stone's photo, base64: a JPEG, PNG or WebP of at most 4 MB.
    pub content_base64: String,
}

/// The colors the AI picked.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoneColors {
    /// Ids from `GET /v1/colors`: the main color first, then the other
    /// colors the stone likely shows, surest first; at most 4. Empty when
    /// the AI could not tell.
    pub color_ids: Vec<i32>,
}

#[derive(sqlx::FromRow)]
struct Color {
    id: i32,
    name: String,
}

/// The photo's type by its first bytes: its MIME type.
fn photo_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The Decisions request: which listed color is the stone's main one, and
/// for each color whether the stone clearly shows it.
fn decision_body(colors: &[Color], photo_url: &str) -> Value {
    let mut choices: Vec<Value> = Vec::new();
    for color in colors {
        if !choices.iter().any(|c| c["value"] == color.name.as_str()) {
            choices.push(json!({ "value": color.name }));
        }
    }
    let mut questions = vec![json!({
        "type": "choice",
        "name": MAIN_QUESTION,
        "instructions": "Which color is the stone's main (background) color?",
        "choices": choices,
    })];
    questions.extend(colors.iter().map(|color| {
        json!({
            "type": "predicate",
            "name": color.id.to_string(),
            "instructions": format!(
                "Does the stone clearly show {}, as its main color or in its veins, flecks or movement?",
                color.name
            ),
        })
    }));
    json!({
        "model": DECISIONS_MODEL,
        "input": [{
            "role": "user",
            "content": [
                { "type": "input_text", "text": CONTEXT },
                { "type": "input_image", "image_url": photo_url, "detail": "auto" },
            ],
        }],
        "questions": questions,
    })
}

/// The picked ids out of a Decisions reply: the main color first, then the
/// colors the stone likely shows, surest first, at most [`MAX_PICKED`].
/// None when the reply has no answers.
fn picked_ids(reply: &Value, colors: &[Color]) -> Option<Vec<i32>> {
    let answers = reply.get("answers")?.as_array()?;
    let mut picked = Vec::new();
    let main = answers
        .iter()
        .find(|a| a["type"] == "choice" && a["name"] == MAIN_QUESTION)
        .and_then(|a| a["choice"].as_str())
        .and_then(|name| colors.iter().find(|color| color.name == name));
    if let Some(main) = main {
        picked.push(main.id);
    }
    let mut shown: Vec<(i32, f64)> = answers
        .iter()
        .filter(|a| a["type"] == "predicate")
        .filter_map(|a| {
            let id: i32 = a["name"].as_str()?.parse().ok()?;
            let probability = a["probability"].as_f64()?;
            colors
                .iter()
                .any(|color| color.id == id)
                .then_some((id, probability))
        })
        .filter(|(_, probability)| *probability >= SHOWS_COLOR_PROBABILITY)
        .collect();
    shown.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (id, _) in shown {
        if !picked.contains(&id) {
            picked.push(id);
        }
    }
    picked.truncate(MAX_PICKED);
    Some(picked)
}

/// Pick a stone's colors from its photo with AI
///
/// Reads a JPEG, PNG or WebP photo of a stone (at most 4 MB, sent base64)
/// and returns the colors from `GET /v1/colors` that it shows: the main
/// color first, then up to 3 more, surest first. Nothing is saved. Company
/// admins (and super admins, superusers) only.
#[crate::openapi::path(
    post,
    path = "/v1/ai/stone-colors",
    operation_id = "pick_stone_colors",
    tag = "ai",
    request_body = StoneColorsInput,
    responses(
        (status = 200, description = "The colors the photo shows", body = StoneColors),
        (status = 400, description = "Not base64, empty, too big, or not a JPEG, PNG or WebP", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 502, description = "OpenAI failed or answered without colors", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pick_stone_colors(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<StoneColorsInput>,
) -> ApiResult<Json<StoneColors>> {
    let bytes = STANDARD
        .decode(input.content_base64.trim())
        .map_err(|_| ApiError::BadRequest("The photo did not arrive whole. Try again.".into()))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("The photo is empty.".into()));
    }
    if bytes.len() > MAX_PHOTO_BYTES {
        return Err(ApiError::BadRequest(
            "The photo is too big: 4 MB at most.".into(),
        ));
    }
    let mime = photo_type(&bytes)
        .ok_or_else(|| ApiError::BadRequest("Upload a JPEG, PNG or WebP photo.".into()))?;
    let colors: Vec<Color> =
        sqlx::query_as("SELECT id, name FROM colors ORDER BY name ASC, id ASC")
            .fetch_all(&state.pool)
            .await?;
    if colors.len() < 2 {
        // A choice question needs two options; a single color is no pick.
        return Ok(Json(StoneColors::default()));
    }
    let url = format!("data:{mime};base64,{}", STANDARD.encode(&bytes));
    let caller = Caller {
        feature: "stone_colors",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = state
        .ai
        .decide(
            &state.pool,
            caller,
            &decision_body(&colors, &url),
            Duration::from_secs(25),
        )
        .await
        .map_err(|err| ApiError::Upstream(err.to_string()))?;
    let color_ids = picked_ids(&reply, &colors).ok_or_else(|| {
        ApiError::Upstream("Could not tell the stone's colors. Pick them by hand.".into())
    })?;
    Ok(Json(StoneColors { color_ids }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Vec<Color> {
        [(2, "Black"), (7, "Gray"), (15, "White"), (6, "Gold"), (4, "Brown")]
            .into_iter()
            .map(|(id, name)| Color {
                id,
                name: name.into(),
            })
            .collect()
    }

    #[test]
    fn the_main_color_comes_first_then_the_surest_shown_ones() {
        let reply = json!({ "answers": [
            { "type": "choice", "name": "main", "choice": "Gray", "confidence": 0.8 },
            { "type": "predicate", "name": "2", "probability": 0.3 },
            { "type": "predicate", "name": "7", "probability": 0.99 },
            { "type": "predicate", "name": "15", "probability": 0.7 },
            { "type": "predicate", "name": "6", "probability": 0.9 },
            { "type": "predicate", "name": "4", "probability": 0.6 },
            { "type": "predicate", "name": "99", "probability": 0.99 },
            { "type": "refusal", "name": "3" },
        ]});
        assert_eq!(picked_ids(&reply, &colors()), Some(vec![7, 6, 15, 4]));
        let refused = json!({ "answers": [{ "type": "refusal", "name": "main" }] });
        assert_eq!(picked_ids(&refused, &colors()), Some(vec![]));
        assert_eq!(picked_ids(&json!({}), &colors()), None);
    }

    #[test]
    fn the_request_asks_about_every_color() {
        let body = decision_body(&colors(), "data:image/png;base64,QQ==");
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,QQ=="
        );
        let questions = body["questions"].as_array().unwrap();
        assert_eq!(questions.len(), 6);
        assert_eq!(questions[0]["type"], "choice");
        assert_eq!(questions[0]["choices"].as_array().unwrap().len(), 5);
        assert_eq!(questions[1]["name"], "2");
        assert_eq!(questions[1]["type"], "predicate");
    }

    #[test]
    fn photos_are_told_apart_by_their_bytes() {
        assert_eq!(photo_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(photo_type(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(photo_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(photo_type(b"%PDF-1.7"), None);
    }
}
