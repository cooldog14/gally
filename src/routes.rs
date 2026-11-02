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

use crate::db::{key_hash, GalleryDb, MintError};
use crate::state::{is_admin, rate_limited, AppState};
use crate::validate::{is_slug, normalize_key, percent_decode, photo_slug, sanitize_filename, canvas_media, CANVAS_MAX_BODY};

/// Max upload size. `GALLY_MAX_BYTES` overrides (default 10 GiB).
pub fn max_bytes() -> usize {
    std::env::var("GALLY_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024 * 1024)
}

/// OGG is a container: sniff the first content packet for the codec.
/// Returns a video or audio MIME, or None when it's not OGG / unknown.
async fn sniff_ogg(path: &std::path::Path) -> Option<&'static str> {
    let mut buf = [0u8; 128];
    let mut f = tokio::fs::File::open(path).await.ok()?;
    use tokio::io::AsyncReadExt;
    let n = f.read(&mut buf).await.ok()?;
    if n < 8 || &buf[..4] != b"OggS" {
        return None;
    }
    let head = &buf[..n];
    let has = |tag: &[u8]| head.windows(tag.len()).any(|w| w == tag);
    if has(b"theora") || has(b"dirac") || has(b"VP8") || has(b"VP9") || has(b"AV1") {
        return Some("video/ogg");
    }
    if has(b"vorbis") || has(b"OpusHead") || has(b"FLAC") || has(b"Speex") {
        return Some("audio/ogg");
    }
    None
}

fn content_type_for(ext: &str) -> &'static str {
    match ext {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".avif" => "image/avif",
        ".bmp" => "image/bmp",
        ".svg" => "image/svg+xml",
        ".ico" => "image/vnd.microsoft.icon",
        ".heic" | ".heif" => "image/heic",
        ".mp4" => "video/mp4",
        ".webm" => "video/webm",
        ".mov" => "video/quicktime",
        ".ogv" => "video/ogg",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" => "audio/ogg",
        ".flac" => "audio/flac",
        ".m4a" => "audio/mp4",
        ".pdf" => "application/pdf",
        ".txt" => "text/plain",
        ".json" => "application/json",
        ".md" => "text/markdown",
        _ => "application/octet-stream",
    }
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    TooLarge,
    TooMany,
    Internal,
}

impl ApiError {
    fn status_and_msg(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "gallery key required"),
            ApiError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "file too large"),
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

fn client_ip(_headers: &HeaderMap, info: Option<ConnectInfo<std::net::SocketAddr>>) -> IpAddr {
    // Never trust X-Forwarded-For: any client can spoof it and dodge the
    // rate limit. Only the real connection address counts.
    info.map(|c| c.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

fn request_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gallery-key")
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_key)
}

/// Target gallery for uploads. Absent on home (= mint a fresh gallery).
fn request_slug(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gallery-slug")
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_slug(s))
        .map(|s| s.to_string())
}

/// Viewer password from header or query. Empty when absent.
fn request_pass(headers: &HeaderMap, query: &str) -> String {
    if let Some(p) = headers.get("x-gallery-pass").and_then(|v| v.to_str().ok()) {
        if !p.is_empty() {
            return p.to_string();
        }
    }
    for part in query.split('&') {
        let (k, v) = match part.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        if k == "pass" {
            return percent_decode(v);
        }
    }
    String::new()
}

/// Slow Argon2 work runs on the blocking pool: never stall the runtime.
/// `None` = open (public gallery or correct password / owner bypass).
/// `Some(response)` = locked, return it directly.
async fn check_locked(state: &AppState, slug: &str, pass: &str) -> Option<Response> {
    let stored = {
        let db = state.db.lock().unwrap();
        db.gallery_pass_hash(slug)
    };
    let hash = match stored {
        None => return None,
        Some(h) => h,
    };
    // The gallery key always opens its own gallery (owner bypass).
    if !pass.is_empty() {
        let pass = pass.to_string();
        let ok = tokio::task::spawn_blocking(move || crate::db::verify_password(&pass, &hash))
            .await
            .unwrap_or(false);
        if ok {
            return None;
        }
    }
    Some((
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "gallery locked", "locked": true })),
    )
        .into_response())
}

/// True when gallery creation is allowed: no invite configured, or the
/// request carries the invite code (constant-time compare).
fn invite_ok(state: &AppState, headers: &HeaderMap) -> bool {
    let guard = state.invite.lock().unwrap();
    let Some(expected) = guard.as_deref() else {
        return true;
    };
    let got = headers
        .get("x-invite-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    use subtle::ConstantTimeEq;
    got.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1
}

// --- canvas document ---

/// One freeform canvas per gallery. Viewers need the same credentials as the
/// photo listing (the owner key bypasses the viewer lock).
pub async fn get_canvas(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let doc = {
        let db = state.db.lock().unwrap();
        if !db.gallery_exists(&slug) {
            return err(StatusCode::NOT_FOUND, "unknown gallery");
        }
        db.get_canvas(&slug)
    };
    let key_ok = request_key(&headers)
        .map(|k| {
            let db = state.db.lock().unwrap();
            db.key_owns_slug(&k, &slug)
        })
        .unwrap_or(false);
    if !key_ok {
        let query = raw.unwrap_or_default();
        let pass = request_pass(&headers, &query);
        if let Some(deny) = check_locked(&state, &slug, &pass).await {
            return deny;
        }
    }
    let body = doc.unwrap_or_else(|| r#"{"blocks":[]}"#.to_string());
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Owner-only canvas write. Stores the validated document verbatim.
pub async fn put_canvas(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    body: Body,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let key = match request_key(&headers) {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let owns = {
        let db = state.db.lock().unwrap();
        db.key_owns_slug(&key, &slug)
    };
    if !owns {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let bytes = match axum::body::to_bytes(body, CANVAS_MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, "canvas too large"),
    };
    let doc = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, "invalid canvas"),
    };
    let Some(srcs) = canvas_media(doc, &slug) else {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "invalid canvas");
    };
    // Referenced media must be real files of this gallery. Canonicalized, so
    // a crafted filename cannot reach outside the photos dir.
    for src in srcs {
        let ok = match tokio::fs::canonicalize(state.dir.join(&src)).await {
            Ok(c) if c.starts_with(&state.canon_dir) => {
                tokio::fs::metadata(&c).await.map(|m| m.is_file()).unwrap_or(false)
            }
            _ => false,
        };
        if !ok {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "unknown media");
        }
    }
    let ok = {
        let db = state.db.lock().unwrap();
        db.set_canvas(&slug, doc)
    };
    if !ok {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    (StatusCode::OK, Json(json!({ "saved": true }))).into_response()
}

// --- gallery photo listing ---

pub async fn gallery_photos(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let t_total = std::time::Instant::now();
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // No cache: plain scan is fast and never goes stale under concurrent upload.
    let t_db = std::time::Instant::now();
    let exists = {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    tracing::debug!(stage = "list_db_check", latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0));
    if !exists {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // Viewer lock: owner key bypasses, otherwise require the gallery password.
    let key_ok = request_key(&headers)
        .map(|k| {
            let db = state.db.lock().unwrap();
            // Scoped: the key must own THIS slug, not just any gallery.
            db.key_owns_slug(&k, &slug)
        })
        .unwrap_or(false);
    if !key_ok {
        let query = raw.unwrap_or_default();
        let pass = request_pass(&headers, &query);
        if let Some(deny) = check_locked(&state, &slug, &pass).await {
            return deny;
        }
    }
    let t_scan = std::time::Instant::now();
    let result = photo_entries(&state, &slug).await;
    tracing::debug!(stage = "list_dir_scan", latency_ms = format!("{:.2}", t_scan.elapsed().as_secs_f64() * 1000.0));
    match result {
        Ok(photos) => {
            let count = photos.len();
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

#[derive(Deserialize)]
struct UnlockBody {
    #[serde(default)]
    password: String,
}

pub async fn admin_unlock(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if rate_limited(&state, &ip) {
        return ApiError::TooMany.into_response();
    }
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();
    if admin_pass.is_empty() {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let bytes = match axum::body::to_bytes(body, 8192).await {
        Ok(b) => b,
        Err(_) => return ApiError::TooLarge.into_response(),
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
    {
        // Fresh login also sweeps expired tokens so the map never grows.
        let mut tokens = state.admin_tokens.lock().unwrap();
        tokens.retain(|_, issued| issued.elapsed() < crate::state::ADMIN_TOKEN_TTL);
        tokens.insert(tok.clone(), std::time::Instant::now());
    }
    // No `Secure`: the server is plain HTTP, so browsers would refuse to
    // store (and never return) a Secure cookie, silently breaking login.
    let cookie = format!("admin={tok}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400");
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
    #[serde(default)]
    slug: String,
}

/// Home (no slug): always mint a fresh gallery — same key, new gallery.
/// Gallery page (slug given): verify the key owns that slug.
pub async fn resolve_gallery(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if rate_limited(&state, &ip) {
        return ApiError::TooMany.into_response();
    }
    let mut key = request_key(&headers);
    let mut slug_param: Option<String> = None;
    if key.is_none() {
        let bytes = match axum::body::to_bytes(body, 8192).await {
            Ok(b) => b,
            Err(_) => return ApiError::TooLarge.into_response(),
        };
        let parsed: Option<ResolveBody> = serde_json::from_slice(&bytes).ok();
        key = parsed.as_ref().and_then(|b| normalize_key(&b.key));
        slug_param = parsed.and_then(|b| {
            let s = b.slug.trim().to_string();
            if is_slug(&s) { Some(s) } else { None }
        });
    }
    let key = match key {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    if let Some(t) = slug_param {
        let ok = {
            let db = state.db.lock().unwrap();
            db.gallery_exists(&t) && db.key_owns_slug(&key, &t)
        };
        return match ok {
            true => (StatusCode::OK, Json(json!({ "slug": t }))).into_response(),
            false => err(StatusCode::FORBIDDEN, "forbidden"),
        };
    }
    let slug = {
        let db = state.db.lock().unwrap();
        if db.is_banned(&key_hash(&key)) {
            return err(StatusCode::FORBIDDEN, "gallery banned");
        }
        if !invite_ok(&state, &headers) {
            return err(StatusCode::FORBIDDEN, "invite required");
        }
        drop(db);
        let mut db = state.db.lock().unwrap();
        db.get_or_create_gallery(&key)
    };
    match slug {
        Ok(s) => (StatusCode::OK, Json(json!({ "slug": s }))).into_response(),
        Err(MintError::Banned) => err(StatusCode::FORBIDDEN, "gallery banned"),
        Err(MintError::Db) => ApiError::Internal.into_response(),
    }
}

// --- statics + SPA fallback ---

static ADMIN_HTML: &[u8] = include_bytes!("../admin.html");

pub async fn admin_page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html")],
        ADMIN_HTML.to_vec(),
    )
        .into_response()
}

pub async fn admin_page_redirect() -> Response {
    (
        StatusCode::PERMANENT_REDIRECT,
        [(header::LOCATION, "/gally/admin/")],
        "",
    )
        .into_response()
}

pub async fn static_path(State(state): State<Arc<AppState>>, uri: axum::http::Uri) -> Response {
    match state.statics.get(uri.path()) {
        Some((bytes, ctype)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, *ctype),
                (
                    header::CACHE_CONTROL,
                    "no-cache",
                ),
            ],
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
    if slug.contains('/') || slug == "gally" {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let known = is_slug(&slug) && {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    if !known {
        // Unknown, deleted, or legacy slug: send the user home.
        tracing::debug!(stage = "spa_gone", slug = slug.as_str());
        return (
            StatusCode::FOUND,
            [(header::LOCATION, "/")],
            "",
        )
            .into_response();
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
    let mut failed: usize = 0;
    if let Ok(mut rd) = tokio::fs::read_dir(&state.dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            if tokio::fs::remove_file(ent.path()).await.is_ok() {
                removed += 1;
            } else {
                failed += 1;
            }
        }
    }
    // A concurrent upload may land mid-sweep: re-scan, never claim success
    // while files remain.
    let mut remaining = false;
    if let Ok(mut rd) = tokio::fs::read_dir(&state.dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            if ent.file_name().to_string_lossy().starts_with(&prefix) {
                remaining = true;
                break;
            }
        }
    }
    if failed > 0 || remaining {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "deleted": slug, "files": removed, "error": "partial delete" })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "deleted": slug, "files": removed }))).into_response()
}

#[derive(Deserialize)]
struct LockBody {
    #[serde(default)]
    password: String,
}

/// Set (non-empty password) or clear (empty/missing) the gallery viewer lock.
/// Requires the gallery key. Passwords are Argon2id-hashed, never stored raw.
pub async fn lock_gallery(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    body: Body,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let key = match request_key(&headers) {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let known = {
        let db = state.db.lock().unwrap();
        db.key_owns_slug(&key, slug.as_str())
    };
    if !known {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let bytes = match axum::body::to_bytes(body, 8192).await {
        Ok(b) => b,
        // Oversize and stream errors both surface here; oversize is the
        // only realistic cause, so report 413 (ApiError::TooLarge).
        Err(_) => return ApiError::TooLarge.into_response(),
    };
    // Malformed JSON never silently clears the lock: reject it.
    let password: String = match serde_json::from_slice::<LockBody>(&bytes) {
        Ok(b) => b.password,
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad request"),
    };
    let hash = if password.is_empty() {
        None
    } else {
        // Argon2 is slow by design: hash off the async runtime.
        let h = tokio::task::spawn_blocking(move || crate::db::hash_password(&password))
            .await
            .unwrap_or_default();
        if h.is_empty() {
            return ApiError::Internal.into_response();
        }
        Some(h)
    };
    let ok = {
        let mut db = state.db.lock().unwrap();
        db.set_gallery_pass(&slug, hash.as_deref())
    };
    if !ok {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    (StatusCode::OK, Json(json!({ "slug": slug, "locked": hash.is_some() }))).into_response()
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
        Err(_) => return ApiError::TooLarge.into_response(),
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
    let t_db = std::time::Instant::now();
    let target = request_slug(&headers);
    let slug = {
        let db = state.db.lock().unwrap();
        match (&target, &key) {
            // Gallery page: key must own this slug. Same password on two
            // galleries stays two galleries. Banned keys stay banned.
            (Some(t), k) => {
                if db.is_banned(&key_hash(k)) {
                    return err(StatusCode::FORBIDDEN, "gallery banned");
                }
                if !db.gallery_exists(t) {
                    return err(StatusCode::NOT_FOUND, "unknown gallery");
                }
                if !db.key_owns_slug(k, t) {
                    return err(StatusCode::FORBIDDEN, "forbidden");
                }
                t.clone()
            }
            // Home: always mint a fresh gallery, never reuse.
            (None, k) => {
                if db.is_banned(&key_hash(k)) {
                    return err(StatusCode::FORBIDDEN, "gallery banned");
                }
                if !invite_ok(&state, &headers) {
                    return err(StatusCode::FORBIDDEN, "invite required");
                }
                drop(db);
                let mut db = state.db.lock().unwrap();
                match db.get_or_create_gallery(k) {
                    Ok(s) => s,
                    Err(MintError::Banned) => {
                        return err(StatusCode::FORBIDDEN, "gallery banned");
                    }
                    Err(MintError::Db) => return ApiError::Internal.into_response(),
                }
            }
        }
    };
    tracing::debug!(stage = "db_get_or_create", latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0));
    let (sanitized, _ext) = sanitize_filename(&raw_name);
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
        if total > max_bytes() {
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
    (StatusCode::CREATED, Json(json!({ "name": stored, "slug": slug }))).into_response()
}
// --- photo serve / delete ---

pub async fn get_photo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let t_total = std::time::Instant::now();
    // Orphaned/banned files stay on disk but are unreachable: reject names
    // outside a known gallery before touching the filesystem.
    let slug = match photo_slug(&name) {
        Some(s) => s.to_string(),
        None => return err(StatusCode::NOT_FOUND, "not found"),
    };
    let gallery_known = {
        let db = state.db.lock().unwrap();
        db.gallery_exists(&slug)
    };
    if !gallery_known {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    // ?pass= in <img> URLs stays: media tags cannot set headers, so the
    // query secret is the only way to authorize direct photo embeds.
    let query = raw.unwrap_or_default();
    let pass = request_pass(&headers, &query);
    // Viewer lock applies per-file too (direct /photos/ links).
    let key_ok = request_key(&headers)
        .map(|k| {
            let db = state.db.lock().unwrap();
            db.key_owns_slug(&k, &slug)
        })
        .unwrap_or(false);
    if !key_ok {
        if let Some(deny) = check_locked(&state, &slug, &pass).await {
            return deny;
        }
    }
    // Private cache when the gallery is locked and the viewer proved it with
    // a password (header or ?pass=): never let a shared cache serve it.
    let locked_gallery = {
        let db = state.db.lock().unwrap();
        db.gallery_pass_hash(&slug).is_some()
    };
    let cache_control = if locked_gallery && !pass.is_empty() {
        "private, no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
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
    // .ogg is ambiguous (Theora video vs Vorbis/Opus audio): sniff content.
    let ctype = if ext == ".ogg" || ext == ".ogv" || ext == ".ogm" || ext == ".opus" {
        sniff_ogg(&canon_file).await.unwrap_or_else(|| content_type_for(ext.as_str()))
    } else {
        content_type_for(ext.as_str())
    }
    .to_string();
    tracing::debug!(stage = "serve_total", latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
    let stream = tokio_util::io::ReaderStream::new(file);
    // SVG runs in site origin: sandbox it and force download.
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::CACHE_CONTROL, cache_control)
        .header("x-content-type-options", "nosniff");
    if ext == ".svg" {
        builder = builder
            .header(header::CONTENT_SECURITY_POLICY, "sandbox")
            .header(header::CONTENT_DISPOSITION, "attachment");
    }
    builder.body(Body::from_stream(stream)).unwrap()
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
                db.key_owns_slug(&k, slug.as_str())
            }
            None => false,
        };
        if !ok {
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
    }
    let path = state.dir.join(&name);
    match path.canonicalize() {
        Ok(canon) if canon.starts_with(&state.canon_dir) => {}
        _ => return err(StatusCode::NOT_FOUND, "not found"),
    }
    match tokio::fs::remove_file(&path).await {
        // Keep the gallery row: a concurrent upload landing between a file
        // count and a row delete orphaned files under a dead slug.
        Ok(()) => (StatusCode::OK, Json(json!({ "deleted": name, "gallery_gone": false }))).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            err(StatusCode::NOT_FOUND, "not found")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "cannot delete"),
    }
}

// --- compat stubs ---

pub async fn gone_compat() -> Response {
    err(StatusCode::GONE, "gone")
}

pub async fn fallback_404() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}
