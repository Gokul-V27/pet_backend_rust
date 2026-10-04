//! Staff picture uploads. `POST /api/v1/admin/images` (multipart: `file`, optional `folder`)
//! → `{ key, url }`. `DELETE /api/v1/admin/images/{*key}` removes one this server made.
//! Needs a staff session whose role may manage the catalogue.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    routing::{delete, post},
};
use serde_json::{Value, json};

use crate::auth::AdminAuth;
use crate::error::{AppError, Result};
use crate::repo::audit;
use crate::services::storage::{self, ImageKind, MAX_IMAGE_BYTES, Storage};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/images", post(upload))
        .route("/images/{*key}", delete(remove))
        // Room for one 5 MB picture plus the form wrapping; everything else stays at the 1 MB default.
        .layer(DefaultBodyLimit::max(MAX_IMAGE_BYTES + 64 * 1024))
}

fn storage_of(s: &AppState) -> Result<&Storage> {
    s.storage
        .as_ref()
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("image storage is not configured")))
}

fn allowed(admin: &AdminAuth) -> Result<()> {
    if admin.role.can_manage_catalogue() {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

async fn upload(
    State(s): State<AppState>,
    admin: AdminAuth,
    mut form: Multipart,
) -> Result<Json<Value>> {
    allowed(&admin)?;
    let store = storage_of(&s)?;

    let mut folder = "products".to_owned();
    let mut file = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| AppError::invalid("could not read the upload"))?
    {
        match field.name() {
            Some("folder") => {
                folder = field
                    .text()
                    .await
                    .map_err(|_| AppError::invalid("bad folder"))?
            }
            Some("file") => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| AppError::invalid("picture too large (5 MB max)"))?;
                file = Some(bytes);
            }
            _ => {}
        }
    }
    if !storage::FOLDERS.contains(&folder.as_str()) {
        return Err(AppError::invalid("unknown folder"));
    }
    let bytes = file.ok_or_else(|| AppError::invalid("choose a picture to upload"))?;
    if bytes.is_empty() {
        return Err(AppError::invalid("the file is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AppError::invalid("picture too large (5 MB max)"));
    }
    let kind = ImageKind::sniff(&bytes)
        .ok_or_else(|| AppError::invalid("use a JPEG, PNG or WebP picture"))?;

    let key = storage::new_key(&folder, kind, chrono::Utc::now());
    let size = bytes.len();
    store.put_image(&key, kind, bytes).await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "image.upload",
        Some(&key),
        json!({ "bytes": size, "type": kind.mime() }),
    )
    .await;
    Ok(Json(json!({ "key": key, "url": store.public_url(&key) })))
}

async fn remove(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(key): Path<String>,
) -> Result<Json<Value>> {
    allowed(&admin)?;
    if !storage::valid_key(&key) {
        return Err(AppError::invalid("not a picture this server manages"));
    }
    storage_of(&s)?.delete(&key).await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "image.delete",
        Some(&key),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "deleted": key })))
}
