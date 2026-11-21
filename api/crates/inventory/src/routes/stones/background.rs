//! Slab picture background (Admin › Stones › Settings).
//!
//! The company's own picture (a warehouse, a rack, its logo) with a four-corner
//! spot marked on it. Every stone picture uploaded while it is set is placed
//! in that spot (`crate::slab_background`); the stone keeps the picture as
//! uploaded in `original_url`.
//!
//! - `PUT /v1/stones/settings/slab-background` → set the picture and spot
//! - `DELETE /v1/stones/settings/slab-background` → remove it
//! - `POST /v1/stones/settings/slab-background/apply` → remake a few stone
//!   pictures made with an older background; call until `pending` is 0
//!
//! Pictures uploaded before a background was set are left as they are. A
//! stone's close-up (`stones.closeup_url`) is drawn in a circle over the
//! finished picture (`crate::stone_closeup`), with or without a background.

use super::queries::closeup_place;
use super::schemas::CloseupPlace;
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::images;
use crate::openapi::ToSchema;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::slab_background::{Spot, frame, valid_spot};
use crate::state::AppState;
use crate::stone_closeup::add_closeup;
use crate::storage::{Storage, icon_key};
use crate::thumbnails::make_icon;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::MySqlConnection;

/// Stone pictures remade by one `apply` call (each takes about a second).
const APPLY_BATCH: i64 = 4;

/// The company's slab picture background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SlabBackground {
    /// The background picture: the `url` of an upload to the `stones` folder
    /// (`POST /v1/uploads`).
    pub url: String,
    /// Where the stone picture goes: four `[x, y]` corners as fractions of
    /// the picture (0 to 1), clockwise from top left.
    #[schema(value_type = Vec<Vec<f64>>)]
    pub spot: Spot,
}

/// How many stone pictures still show an older background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct PendingPictures {
    /// Stone pictures `POST /v1/stones/settings/slab-background/apply` has
    /// yet to remake.
    pub pending: i64,
}

/// The company's background and the version stone pictures are made with.
pub struct CompanyBackground {
    /// `None` when the company has none.
    pub background: Option<SlabBackground>,
    pub version: i32,
}

/// What a stone stores for a picture that was just uploaded.
pub struct StonePicture {
    /// The picture shown everywhere: on the background when the company has
    /// one, else as uploaded.
    pub url: String,
    /// The picture as uploaded, when `url` is on the background.
    pub original_url: Option<String>,
    /// The background version `url` was made with.
    pub version: Option<i32>,
}

pub async fn company_background(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> Result<CompanyBackground, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>, i32)> = sqlx::query_as(
        "SELECT slab_background_url, slab_background_spot, slab_background_version
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (url, spot, version) = row.unwrap_or((None, None, 0));
    let background = url.zip(spot).and_then(|(url, spot)| {
        let spot: Spot = serde_json::from_str(&spot).ok()?;
        valid_spot(&spot).then_some(SlabBackground { url, spot })
    });
    Ok(CompanyBackground {
        background,
        version,
    })
}

/// Stone pictures of the company made with another background version.
pub async fn pending_pictures(
    conn: &mut MySqlConnection,
    company_id: i32,
    version: i32,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM stones
          WHERE company_id = ? AND deleted_at IS NULL AND original_url IS NOT NULL
            AND NOT (slab_background_version <=> ?)",
    )
    .bind(company_id)
    .bind(version)
    .fetch_one(&mut *conn)
    .await
}

/// The stone picture at `picture_url` as shown: on the background when the
/// company has one, with the close-up in a red circle when the stone has one.
/// Stored as a new file of the company with its thumbnail: its URL. `None`
/// when there is nothing to add, or a file is missing or not a picture this
/// can read.
async fn made_copy(
    storage: &Storage,
    background: Option<&SlabBackground>,
    picture_url: &str,
    closeup_url: Option<&str>,
    place: Option<CloseupPlace>,
    company_id: i32,
) -> ApiResult<Option<String>> {
    if background.is_none() && closeup_url.is_none() {
        return Ok(None);
    }
    let Some(picture) = storage.get_url(picture_url).await? else {
        return Ok(None);
    };
    let back = match background {
        Some(background) => match storage.get_url(&background.url).await? {
            Some(file) => Some((file.bytes, background.spot)),
            None => return Ok(None),
        },
        None => None,
    };
    let closeup = match closeup_url {
        Some(url) => match storage.get_url(url).await? {
            Some(file) => Some(file.bytes),
            None => return Ok(None),
        },
        None => None,
    };
    let made = tokio::task::spawn_blocking(move || {
        let mut bytes = picture.bytes;
        if let Some((back, spot)) = back {
            bytes = frame(&back, &bytes, &spot)?;
        }
        if let Some(closeup) = closeup {
            bytes = add_closeup(&bytes, &closeup, place.map(Into::into))?;
        }
        let icon = make_icon(&bytes);
        Some((bytes, icon))
    })
    .await
    .ok()
    .flatten();
    let Some((bytes, icon)) = made else {
        return Ok(None);
    };
    let key = uploads::new_key(UploadFolder::Stones, company_id, ".jpg");
    storage.put(&key, bytes, "image/jpeg").await?;
    if let Some(icon) = icon {
        storage
            .put(&icon_key(&key), icon.bytes, icon.content_type)
            .await?;
    }
    Ok(Some(storage.public_url(&key)?))
}

/// What to store for the stone picture `uploaded` (as uploaded): on the
/// company's background when it has one, with `closeup` in a circle when
/// given, as long as the files can be read.
pub async fn stone_picture(
    storage: &Storage,
    conn: &mut MySqlConnection,
    company_id: i32,
    uploaded: &str,
    closeup: Option<&str>,
    place: Option<CloseupPlace>,
) -> ApiResult<StonePicture> {
    let company = company_background(conn, company_id).await?;
    let made = made_copy(
        storage,
        company.background.as_ref(),
        uploaded,
        closeup,
        place,
        company_id,
    )
    .await?;
    Ok(match made {
        Some(url) => StonePicture {
            url,
            original_url: Some(uploaded.to_string()),
            version: Some(company.version),
        },
        None => StonePicture {
            url: uploaded.to_string(),
            original_url: None,
            version: None,
        },
    })
}

/// Deletes a stone picture file no stone shows any more.
pub async fn remove_unused(
    storage: &Storage,
    conn: &mut MySqlConnection,
    url: &str,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    let used: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM stones WHERE url = ? OR original_url = ? OR closeup_url = ? LIMIT 1",
    )
    .bind(url)
    .bind(url)
    .bind(url)
    .fetch_optional(&mut *conn)
    .await?;
    if used.is_none() {
        uploads::remove(storage, url, company_id).await;
    }
    Ok(())
}

async fn current_url(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let url: Option<Option<String>> =
        sqlx::query_scalar("SELECT slab_background_url FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(url.flatten())
}

/// Set the slab picture background
///
/// Admin › Stones › Settings. Saves the company's background picture (an
/// upload to the `stones` folder) and the spot on it. Every stone picture
/// uploaded from now on is placed in that spot; the ones already made with
/// an older background are counted in `pending` and remade by
/// `POST /v1/stones/settings/slab-background/apply`. Pictures uploaded
/// before a background was set stay as they are.
#[crate::openapi::path(
    put,
    path = "/v1/stones/settings/slab-background",
    operation_id = "save_slab_background",
    tag = "stones",
    request_body = SlabBackground,
    responses(
        (status = 200, description = "Saved", body = PendingPictures),
        (status = 400, description = "Not an uploaded picture, or the corners are outside the picture, crossed or too close together", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<SlabBackground>,
) -> ApiResult<Json<PendingPictures>> {
    if !valid_spot(&input.spot) {
        return Err(ApiError::BadRequest(
            "Put the four corners on the place where the slab stands, without crossing them."
                .into(),
        ));
    }
    let mut conn = state.pool.acquire().await?;
    let old = current_url(&mut conn, user.company_id).await?;
    let replaced = old.as_deref() != Some(input.url.as_str());
    if replaced {
        uploads::attach(
            &state.storage,
            &input.url,
            UploadFolder::Stones,
            user.company_id,
        )
        .await?;
        let readable = match state.storage.get_url(&input.url).await? {
            Some(file) => tokio::task::spawn_blocking(move || images::decode(&file.bytes).is_ok())
                .await
                .unwrap_or(false),
            None => false,
        };
        if !readable {
            return Err(ApiError::BadRequest(
                "This file is not a picture. Upload a JPG or PNG.".into(),
            ));
        }
    }
    let spot = serde_json::to_string(&input.spot).unwrap_or_default();
    sqlx::query(
        "UPDATE company SET slab_background_url = ?, slab_background_spot = ?,
                slab_background_version = slab_background_version + 1
          WHERE id = ?",
    )
    .bind(&input.url)
    .bind(spot)
    .bind(user.company_id)
    .execute(&mut *conn)
    .await?;
    if let (true, Some(old)) = (replaced, &old) {
        uploads::remove(&state.storage, old, user.company_id).await;
    }
    pending(&mut conn, user.company_id).await
}

/// Remove the slab picture background
///
/// New stone pictures are stored as uploaded again. The ones on the
/// background are counted in `pending`:
/// `POST /v1/stones/settings/slab-background/apply` puts them back as they
/// were uploaded.
#[crate::openapi::path(
    delete,
    path = "/v1/stones/settings/slab-background",
    operation_id = "remove_slab_background",
    tag = "stones",
    responses(
        (status = 200, description = "Removed", body = PendingPictures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn remove(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<PendingPictures>> {
    let mut conn = state.pool.acquire().await?;
    let old = current_url(&mut conn, user.company_id).await?;
    if let Some(old) = &old {
        sqlx::query(
            "UPDATE company SET slab_background_url = NULL, slab_background_spot = NULL,
                    slab_background_version = slab_background_version + 1
              WHERE id = ?",
        )
        .bind(user.company_id)
        .execute(&mut *conn)
        .await?;
        uploads::remove(&state.storage, old, user.company_id).await;
    }
    pending(&mut conn, user.company_id).await
}

/// Remake stone pictures for the current background
///
/// Remakes up to four stone pictures that were made with an older
/// background: on the current one, or back as uploaded when the company
/// removed its background (the stone's close-up circle stays). Call it again while `pending` is above 0.
#[crate::openapi::path(
    post,
    path = "/v1/stones/settings/slab-background/apply",
    operation_id = "apply_slab_background",
    tag = "stones",
    responses(
        (status = 200, description = "A few pictures remade", body = PendingPictures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn apply(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<PendingPictures>> {
    let mut conn = state.pool.acquire().await?;
    let company = company_background(&mut conn, user.company_id).await?;
    let stale: Vec<(i32, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT id, url, original_url, closeup_url FROM stones
          WHERE company_id = ? AND deleted_at IS NULL AND original_url IS NOT NULL
            AND NOT (slab_background_version <=> ?)
          ORDER BY id LIMIT ?",
    )
    .bind(user.company_id)
    .bind(company.version)
    .bind(APPLY_BATCH)
    .fetch_all(&mut *conn)
    .await?;
    for (id, shown, original, closeup) in stale {
        let place = closeup_place(&mut *conn, id).await?;
        let made = made_copy(
            &state.storage,
            company.background.as_ref(),
            &original,
            closeup.as_deref(),
            place,
            user.company_id,
        )
        .await?;
        let adds_something = company.background.is_some() || closeup.is_some();
        match (adds_something, made) {
            (_, Some(url)) => {
                sqlx::query("UPDATE stones SET url = ?, slab_background_version = ? WHERE id = ?")
                    .bind(url)
                    .bind(company.version)
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
            }
            // A file can no longer be read: the picture stays as it is.
            (true, None) => {
                sqlx::query("UPDATE stones SET slab_background_version = ? WHERE id = ?")
                    .bind(company.version)
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                continue;
            }
            (false, None) => {
                sqlx::query(
                    "UPDATE stones SET url = original_url, original_url = NULL,
                            slab_background_version = NULL
                      WHERE id = ?",
                )
                .bind(id)
                .execute(&mut *conn)
                .await?;
            }
        }
        if let Some(shown) = shown.filter(|shown| *shown != original) {
            remove_unused(&state.storage, &mut conn, &shown, user.company_id).await?;
        }
    }
    pending(&mut conn, user.company_id).await
}

async fn pending(conn: &mut MySqlConnection, company_id: i32) -> ApiResult<Json<PendingPictures>> {
    let version: Option<i32> =
        sqlx::query_scalar("SELECT slab_background_version FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(Json(PendingPictures {
        pending: pending_pictures(conn, company_id, version.unwrap_or(0)).await?,
    }))
}
