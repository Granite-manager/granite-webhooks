//! `POST /v1/ai/stone-colors`: a stone's colors picked by AI from its photo.
//!
//! The AI picks from the color list (`GET /v1/colors`), so the Add Stone and
//! Edit Stone forms fill the Color field when a photo is chosen. The admin
//! can change the pick before saving.

use crate::ai::{AiError, Caller, Chat, LARGE_FALLBACK_MODEL, LARGE_MODEL};
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
use serde_json::json;
use std::time::Duration;

/// The largest photo read; the web app sends it shrunk to about 1 MB.
pub const MAX_PHOTO_BYTES: usize = 4 * 1024 * 1024;
/// The request body: the photo in base64 (4/3 of it) and the JSON around it.
pub const MAX_PHOTO_BODY_BYTES: usize = MAX_PHOTO_BYTES / 3 * 4 + 64 * 1024;
/// Most colors picked for one stone.
const MAX_PICKED: usize = 4;

const PROMPT: &str = r#"This is a photo of a countertop stone slab (granite, quartz, marble, quartzite...) from a stone yard's inventory.

Pick the colors a customer would use to find this stone when filtering the inventory by color, from this list only (id: name):
{COLORS}

Rules:
- Pick the main background color first, then the colors of the veins, flecks or movement that clearly show. One to four colors.
- Judge the stone itself; ignore the background, floor, rack, lighting glare, shadows, labels and people.
- Use only ids from the list.

Reply with only a JSON object: {"colorIds": [ids, main color first]}."#;

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
    /// Ids from `GET /v1/colors`, the main color first; empty when the AI
    /// could not tell.
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

/// The prompt with the color list in it.
fn prompt(colors: &[Color]) -> String {
    let list = colors
        .iter()
        .map(|color| format!("{}: {}", color.id, color.name))
        .collect::<Vec<_>>()
        .join("\n");
    PROMPT.replace("{COLORS}", &list)
}

/// The picked ids out of the AI's reply: only known ids, each once, in the
/// AI's order, at most [`MAX_PICKED`].
fn parse_ids(reply: &str, known: &[i32]) -> Option<Vec<i32>> {
    let cleaned = reply.replace("```json", "").replace("```", "");
    let parsed: serde_json::Value = serde_json::from_str(cleaned.trim()).ok()?;
    let ids = parsed.get("colorIds")?.as_array()?;
    let mut picked = Vec::new();
    for id in ids {
        let id = id
            .as_i64()
            .or_else(|| id.as_str().and_then(|s| s.trim().parse().ok()))
            .and_then(|id| i32::try_from(id).ok());
        if let Some(id) = id
            && known.contains(&id)
            && !picked.contains(&id)
        {
            picked.push(id);
        }
    }
    picked.truncate(MAX_PICKED);
    Some(picked)
}

/// Pick a stone's colors from its photo with AI
///
/// Reads a JPEG, PNG or WebP photo of a stone (at most 4 MB, sent base64)
/// and returns the colors from `GET /v1/colors` that it shows, the main
/// color first. Nothing is saved. Company admins (and super admins,
/// superusers) only.
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
    if colors.is_empty() {
        return Ok(Json(StoneColors::default()));
    }
    let known: Vec<i32> = colors.iter().map(|color| color.id).collect();
    let url = format!("data:{mime};base64,{}", STANDARD.encode(&bytes));
    let content = json!([
        { "type": "image_url", "image_url": { "url": url, "detail": "low" } },
        { "type": "text", "text": prompt(&colors) },
    ]);
    let chat = |model| {
        let mut chat = Chat::new(model);
        chat.messages
            .push(json!({ "role": "user", "content": content.clone() }));
        chat.json = true;
        chat.reasoning_effort = Some("low");
        chat.timeout = Duration::from_secs(25);
        chat
    };
    let caller = Caller {
        feature: "stone_colors",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = match state.ai.chat(&state.pool, caller, &chat(LARGE_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(LARGE_FALLBACK_MODEL))
                .await
        }
        other => other,
    }
    .map_err(|err: AiError| ApiError::Upstream(err.to_string()))?;
    let color_ids = parse_ids(&reply, &known).ok_or_else(|| {
        ApiError::Upstream("Could not tell the stone's colors. Pick them by hand.".into())
    })?;
    Ok(Json(StoneColors { color_ids }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_ids_are_kept_once_in_order() {
        let known = [1, 2, 3, 4, 5, 6];
        assert_eq!(
            parse_ids("```json\n{\"colorIds\": [3, \"1\", 3, 99, 2]}\n```", &known),
            Some(vec![3, 1, 2])
        );
        assert_eq!(
            parse_ids(r#"{"colorIds": [1, 2, 3, 4, 5, 6]}"#, &known),
            Some(vec![1, 2, 3, 4])
        );
        assert_eq!(parse_ids(r#"{"colorIds": []}"#, &known), Some(vec![]));
        assert_eq!(parse_ids("not json", &known), None);
        assert_eq!(parse_ids(r#"{"colors": [1]}"#, &known), None);
    }

    #[test]
    fn photos_are_told_apart_by_their_bytes() {
        assert_eq!(photo_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(photo_type(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(photo_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(photo_type(b"%PDF-1.7"), None);
    }

    #[test]
    fn the_prompt_lists_every_color() {
        let colors = [
            Color { id: 1, name: "White".into() },
            Color { id: 7, name: "Gold".into() },
        ];
        let text = prompt(&colors);
        assert!(text.contains("1: White\n7: Gold"));
        assert!(!text.contains("{COLORS}"));
    }
}
