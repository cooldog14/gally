use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::db::{key_hash, GalleryDb};
use crate::state::{is_admin, rate_limited, AppState};
use crate::validate::{is_slug, normalize_key, photo_slug, sanitize_filename};

pub const MAX_BYTES: usize = 20 * 1024 * 1024;

static ALLOWED_EXTS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp"];

fn content_type_for(ext: &str) -> &'static str {
    match ext {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug)]
pub enum ApiError {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    TooLarge,
    UnsupportedType,
    TooMany,
    Internal,
}

impl ApiError {
    fn status_and_msg(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest => (StatusCode::BAD_REQUEST, "bad request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "gallery key required"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found"),
            ApiError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "file too large"),
            ApiError::UnsupportedType => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported type")
            }
            ApiError::TooMany => (StatusCode::TOO_MANY_REQUESTS, "too many uploads"),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = self.status_and_msg();
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn client_ip(headers: &HeaderMap, info: Option<ConnectInfo<std::net::SocketAddr>>) -> IpAddr {
    if let Some(fwd) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
    {
        if let Ok(ip) = fwd.trim().parse::<IpAddr>() {
            return ip;
        }
    }
    info.map(|c| c.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

fn request_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gallery-key")
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_key)
}

// --- gallery photo listing ---

pub async fn gallery_photos(
    State(state): State<Arc<AppState>>,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    let t_total = std::time::Instant::now();
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // Cached fast path: skip DB + directory scan.
    if let Some(cached) = state.listings.lock().unwrap().get(&slug) {
        let photos = cached.photos.clone();
        tracing::debug!(stage = "list_cached", count = photos.len(), latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
        return (StatusCode::OK, Json(json!({ "slug": slug, "photos": photos })))
            .into_response();
    }
    let t_db = std::time::Instant::now();
    let exists = {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    tracing::debug!(stage = "list_db_check", latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0));
    if !exists {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let t_scan = std::time::Instant::now();
    let result = photo_entries(&state, &slug).await;
    tracing::debug!(stage = "list_dir_scan", latency_ms = format!("{:.2}", t_scan.elapsed().as_secs_f64() * 1000.0));
    match result {
        Ok(photos) => {
            let count = photos.len();
            state.listings.lock().unwrap().insert(
                slug.clone(),
                crate::state::CachedListing { photos: photos.clone() },
            );
            tracing::debug!(stage = "list_total", count = count, latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
            (StatusCode::OK, Json(json!({ "slug": slug, "photos": photos })))
                .into_response()
        }
        Err(_) => ApiError::Internal.into_response(),
    }
}

async fn photo_entries(
    state: &AppState,
    slug: &str,
) -> std::io::Result<Vec<serde_json::Value>> {
    let prefix = format!("{slug}_");
    let mut out: Vec<(std::time::SystemTime, String)> = Vec::new();
    let mut rd = tokio::fs::read_dir(&state.dir).await?;
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !name.starts_with(&prefix) {
            continue;
        }
        let md = ent.metadata().await?;
        if !md.is_file() {
            continue;
        }
        out.push((md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), name));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(out.into_iter().map(|(_, n)| json!({ "name": n })).collect())
}

// --- admin ---

pub async fn admin_galleries(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let mut rd = match tokio::fs::read_dir(&state.dir).await {
        Ok(r) => r,
        Err(_) => return ApiError::Internal.into_response(),
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if let Some(slug) = photo_slug(&name) {
            *counts.entry(slug.to_string()).or_default() += 1;
        }
    }
    let rows: Vec<(String, i64)> = {
        let db = state.db.lock().unwrap();
        match db.prepare("SELECT slug, created FROM galleries ORDER BY created DESC") {
            Ok(mut stmt) => stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    let galleries: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(slug, created)| {
            json!({ "slug": slug, "created": created, "photos": counts.get(&slug).copied().unwrap_or(0) })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "galleries": galleries }))).into_response()
}

pub async fn admin_banned(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let rows: Vec<i64> = {
        let db = state.db.lock().unwrap();
        match db.prepare("SELECT created FROM banned_keys ORDER BY created DESC") {
            Ok(mut stmt) => stmt
                .query_map([], |r| r.get::<_, i64>(0))
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    let banned: Vec<serde_json::Value> =
        rows.into_iter().map(|c| json!({ "created": c })).collect();
    (StatusCode::OK, Json(json!({ "banned": banned }))).into_response()
}

pub async fn api_me() -> Response {
    err(StatusCode::UNAUTHORIZED, "unauthorized")
}

#[derive(Deserialize)]
struct UnlockBody {
    #[serde(default)]
    password: String,
}

pub async fn admin_unlock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let _ = headers;
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();
    if admin_pass.is_empty() {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let bytes = match axum::body::to_bytes(body, 8192).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::FORBIDDEN, "forbidden"),
    };
    let guess: String = serde_json::from_slice::<UnlockBody>(&bytes)
        .map(|b| b.password)
        .unwrap_or_default();
    use subtle::ConstantTimeEq;
    let ok = guess.as_bytes().ct_eq(admin_pass.as_bytes()).unwrap_u8() == 1;
    if !ok {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let tok = uuid::Uuid::new_v4().as_simple().to_string()
        + &uuid::Uuid::new_v4().as_simple().to_string();
    state.admin_tokens.lock().unwrap().insert(tok.clone());
    let cookie = format!("admin={tok}; HttpOnly; SameSite=Lax; Path=/");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct ResolveBody {
    #[serde(default)]
    key: String,
}

pub async fn resolve_gallery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let mut key = request_key(&headers);
    if key.is_none() {
        let bytes = match axum::body::to_bytes(body, 8192).await {
            Ok(b) => b,
            Err(_) => return ApiError::Unauthorized.into_response(),
        };
        key = serde_json::from_slice::<ResolveBody>(&bytes)
            .ok()
            .and_then(|b| normalize_key(&b.key));
    }
    let key = match key {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let slug = {
        let mut db = state.db.lock().unwrap();
        db.get_or_create_gallery(&key)
    };
    match slug {
        Some(s) => (StatusCode::OK, Json(json!({ "slug": s }))).into_response(),
        None => err(StatusCode::FORBIDDEN, "gallery banned"),
    }
}

// --- statics + SPA fallback ---

pub async fn static_path(State(state): State<Arc<AppState>>, uri: axum::http::Uri) -> Response {
    match state.statics.get(uri.path()) {
        Some((bytes, ctype)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, *ctype)],
            bytes.to_vec(),
        )
            .into_response(),
        None => err(StatusCode::NOT_FOUND, "not found"),
    }
}

pub async fn spa_or_404(
    State(state): State<Arc<AppState>>,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if slug.contains('/') || !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let exists = {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    if !exists {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    match state.statics.get("/index.html") {
        Some((bytes, ctype)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, *ctype)],
            bytes.to_vec(),
        )
            .into_response(),
        None => ApiError::Internal.into_response(),
    }
}

pub async fn delete_gallery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let banned = {
        let mut db = state.db.lock().unwrap();
        db.ban_gallery(&slug)
    };
    if !banned {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let prefix = format!("{slug}_");
    let mut removed: usize = 0;
    if let Ok(mut rd) = tokio::fs::read_dir(&state.dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            if tokio::fs::remove_file(ent.path()).await.is_ok() {
                removed += 1;
            }
        }
    }
    state.invalidate_listing(&slug);
    (StatusCode::OK, Json(json!({ "deleted": slug, "files": removed }))).into_response()
}

#[derive(Deserialize)]
struct UnbanBody {
    #[serde(default)]
    key: String,
}

pub async fn admin_unban(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let bytes = match axum::body::to_bytes(body, 8192).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::BAD_REQUEST, "gallery key required"),
    };
    let key = serde_json::from_slice::<UnbanBody>(&bytes)
        .ok()
        .and_then(|b| normalize_key(&b.key));
    let key = match key {
        Some(k) => k,
        None => return err(StatusCode::BAD_REQUEST, "gallery key required"),
    };
    let digest = key_hash(&key);
    let unbanned = {
        let mut db = state.db.lock().unwrap();
        db.unban(&digest)
    };
    if !unbanned {
        return err(StatusCode::NOT_FOUND, "key not banned");
    }
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// --- PUT upload ---

pub async fn put_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(raw_name): AxumPath<String>,
    body: Body,
) -> Response {
    let t_total = std::time::Instant::now();
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if rate_limited(&state, &ip) {
        return ApiError::TooMany.into_response();
    }
    let key = match request_key(&headers) {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ctype.starts_with("image/") {
        return err(StatusCode::BAD_REQUEST, "not an image");
    }
    let t_db = std::time::Instant::now();
    let slug = {
        let mut db = state.db.lock().unwrap();
        db.get_or_create_gallery(&key)
    };
    tracing::debug!(stage = "db_get_or_create", latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0));
    let slug = match slug {
        Some(s) => s,
        None => return err(StatusCode::FORBIDDEN, "gallery banned"),
    };
    let (sanitized, ext) = sanitize_filename(&raw_name);
    if !ALLOWED_EXTS.contains(&ext.as_str()) {
        return ApiError::UnsupportedType.into_response();
    }
    let t_body = std::time::Instant::now();
    // Stream the body straight to the O_EXCL file: bounded RAM (one frame
    // in flight), enforced MAX_BYTES cap, cleanup of partial files.
    let mut stored = String::new();
    let mut total: usize = 0;
    let mut out: Option<tokio::fs::File> = None;
    let mut stream = body.into_data_stream();
    let mut over_limit = false;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => {
                drop(out.take());
                if !stored.is_empty() {
                    let _ = tokio::fs::remove_file(state.dir.join(&stored)).await;
                }
                return ApiError::TooLarge.into_response();
            }
        };
        if chunk.is_empty() {
            continue;
        }
        total += chunk.len();
        if total > MAX_BYTES {
            over_limit = true;
            break;
        }
        if out.is_none() {
            // First bytes: pick the final name now so a retry keeps O_EXCL.
            let mut opened = false;
            for _ in 0..2 {
                let candidate = format!(
                    "{}_{}_{}",
                    slug,
                    uuid::Uuid::new_v4().as_simple(),
                    sanitized
                );
                let path = state.dir.join(&candidate);
                match tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .await
                {
                    Ok(f) => {
                        stored = candidate;
                        out = Some(f);
                        opened = true;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => {
                        return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store");
                    }
                }
            }
            if !opened {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store");
            }
        }
        if let Some(f) = out.as_mut() {
            if f.write_all(&chunk).await.is_err() {
                let _ = tokio::fs::remove_file(state.dir.join(&stored)).await;
                return ApiError::Internal.into_response();
            }
        }
    }
    tracing::debug!(stage = "body_stream", bytes = total, latency_ms = format!("{:.2}", t_body.elapsed().as_secs_f64() * 1000.0));
    if over_limit {
        if !stored.is_empty() {
            let _ = tokio::fs::remove_file(state.dir.join(&stored)).await;
        }
        return ApiError::TooLarge.into_response();
    }
    if total == 0 {
        if !stored.is_empty() {
            let _ = tokio::fs::remove_file(state.dir.join(&stored)).await;
        }
        return err(StatusCode::BAD_REQUEST, "empty body");
    }
    drop(out);
    tracing::debug!(stage = "put_total", latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
    state.invalidate_listing(&slug);
    (StatusCode::CREATED, Json(json!({ "name": stored, "slug": slug }))).into_response()
}
// --- photo serve / delete ---

pub async fn get_photo(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let t_total = std::time::Instant::now();
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let t_canon = std::time::Instant::now();
    let path = state.dir.join(&name);
    // Contain within photos dir (no symlink escape); base canonicalized once.
    let canon_file = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "not found"),
    };
    if !canon_file.starts_with(&state.canon_dir) {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let file = match tokio::fs::File::open(&canon_file).await {
        Ok(f) => f,
        Err(_) => return err(StatusCode::NOT_FOUND, "not found"),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    tracing::debug!(stage = "serve_open", latency_ms = format!("{:.2}", t_canon.elapsed().as_secs_f64() * 1000.0));
    let ext = match name.rfind('.') {
        Some(i) if i > 0 => name[i..].to_lowercase(),
        _ => String::new(),
    };
    let ctype = content_type_for(ext.as_str()).to_string();
    tracing::debug!(stage = "serve_total", latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
    let stream = tokio_util::io::ReaderStream::new(file);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype),
            (header::CONTENT_LENGTH, len.to_string()),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

pub async fn delete_photo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let slug = match photo_slug(&name) {
        Some(s) => s.to_string(),
        None => return err(StatusCode::NOT_FOUND, "not found"),
    };
    let exists = {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    if !exists {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    if !is_admin(&headers, &state) {
        let key = request_key(&headers);
        let ok = match key {
            Some(k) => {
                let db = state.db.lock().unwrap();
                db.lookup_gallery(&k).as_deref() == Some(slug.as_str())
            }
            None => false,
        };
        if !ok {
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
    }
    let path = state.dir.join(&name);
    if let Ok(canon) = path.canonicalize() {
        if !canon.starts_with(&state.canon_dir) {
            return err(StatusCode::NOT_FOUND, "not found");
        }
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            state.invalidate_listing(&slug);
            (StatusCode::OK, Json(json!({ "deleted": name }))).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            err(StatusCode::NOT_FOUND, "not found")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "cannot delete"),
    }
}

// --- compat stubs ---

pub async fn gone_compat() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

pub async fn fallback_404() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}
