use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::db::{key_hash, GalleryDb, GalleryMeta, MintError};
use crate::state::{
    is_admin, pass_fail_exceeded, pass_fail_record, rate_limited, read_limited, AppState,
    OWNER_TOKEN_TTL, PASS_CACHE_TTL,
};
use crate::validate::{
    is_slug, normalize_key, percent_decode_bytes, photo_slug, sanitize_filename,
};

/// Re-check free space at most this often while a large body streams in.
const FREE_CHECK_EVERY: u64 = 64 * 1024 * 1024;
/// A stalled upload socket is dropped after this long without a chunk.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceiling on one upload, however fast the client.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(6 * 3600);

/// Free bytes on the data volume, or `None` when unknown (never blocks).
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `stat` is plain data; the pointer is a valid C string.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Owner media cookie for `slug`, live and belonging to this gallery.
fn owner_cookie_ok(headers: &HeaderMap, state: &AppState, slug: &str) -> bool {
    let Some(tok) = crate::state::cookie_value(headers, &format!("galowner_{slug}")) else {
        return false;
    };
    let mut tokens = state.owner_tokens.lock().unwrap();
    match tokens.get(tok) {
        // Live and bound to this gallery.
        Some((s, at)) if s == slug && at.elapsed() < OWNER_TOKEN_TTL => true,
        // Expired: drop it so the map never grows.
        Some((_, at)) if at.elapsed() >= OWNER_TOKEN_TTL => {
            tokens.remove(tok);
            false
        }
        _ => false,
    }
}

/// Mint an owner media cookie for `slug` (for `<img>`/`<video>`, which cannot
/// send the key header). Returns the full `Set-Cookie` value.
fn issue_owner_token(state: &AppState, slug: &str) -> String {
    let tok = uuid::Uuid::new_v4().as_simple().to_string()
        + &uuid::Uuid::new_v4().as_simple().to_string();
    {
        let mut tokens = state.owner_tokens.lock().unwrap();
        if tokens.len() >= 10_000 {
            tokens.retain(|_, (_, at)| at.elapsed() < OWNER_TOKEN_TTL);
        }
        tokens.insert(tok.clone(), (slug.to_string(), Instant::now()));
    }
    // No `Secure`: plain HTTP server, browsers would drop the cookie.
    format!(
        "galowner_{slug}={tok}; HttpOnly; SameSite=Lax; Path=/photos/; Max-Age=43200"
    )
}

/// Bounded, timeout-guarded JSON body read.
async fn read_body(body: Body, limit: usize) -> Result<axum::body::Bytes, Response> {
    match tokio::time::timeout(Duration::from_secs(10), axum::body::to_bytes(body, limit)).await {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(_)) => Err(ApiError::TooLarge.into_response()),
        Err(_) => Err(err(StatusCode::REQUEST_TIMEOUT, "request timed out")),
    }
}

/// Parse `Range: bytes=start-end` against `len`.
/// `Some(Ok((start, end)))` = serve that slice, `Some(Err(()))` = 416,
/// `None` = serve the whole file (absent or multi-range).
fn parse_range(headers: &HeaderMap, len: u64) -> Option<Result<(u64, u64), ()>> {
    // Media elements send multi-range probes; answer those with the full file.
    let spec = headers
        .get(header::RANGE)?
        .to_str()
        .ok()?
        .trim()
        .strip_prefix("bytes=")?
        .to_string();
    if spec.contains(',') || len == 0 {
        return None;
    }
    // Past this point the header asked for one byte range: malformed specs are
    // unsatisfiable (416), not silently ignored.
    let Some((a, b)) = spec.split_once('-') else {
        return Some(Err(()));
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Some(suffix) = b.parse::<u64>().ok() else {
            return Some(Err(()));
        };
        if suffix == 0 || suffix > len {
            return Some(Err(()));
        }
        return Some(Ok((len - suffix, len - 1)));
    }
    let Some(start) = a.parse::<u64>().ok() else {
        return Some(Err(()));
    };
    if start >= len {
        return Some(Err(()));
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        let Some(e) = b.parse::<u64>().ok() else {
            return Some(Err(()));
        };
        e.min(len - 1)
    };
    if end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
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

fn trust_proxy() -> bool {
    matches!(
        std::env::var("GALLY_TRUST_PROXY").unwrap_or_default().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

fn client_ip(headers: &HeaderMap, info: Option<ConnectInfo<std::net::SocketAddr>>) -> IpAddr {
    // Never trust X-Forwarded-For by default: any client can spoof it and
    // dodge the rate limit. Only the real connection address counts, unless
    // GALLY_TRUST_PROXY is set and the peer is loopback (local reverse
    // proxy): then the rightmost XFF entry (closest proxy hop) is used.
    let peer = info.map(|c| c.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    if trust_proxy() && peer.is_loopback() {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.rsplit(',').next().map(str::trim) {
                if let Ok(ip) = last.parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    peer
}

fn request_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gallery-key")
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_key)
}

/// Target gallery for uploads. Absent on home (= mint a fresh gallery).
/// Present-but-invalid is a client bug: reject, never silently mint.
fn request_slug(headers: &HeaderMap) -> Result<Option<String>, Response> {
    let raw = headers.get("x-gallery-slug").and_then(|v| v.to_str().ok());
    match raw {
        None => Ok(None),
        Some(s) if is_slug(s) => Ok(Some(s.to_string())),
        Some(_) => Err(err(StatusCode::BAD_REQUEST, "bad slug")),
    }
}

/// Viewer password from header or query. Empty when absent (or not UTF-8).
fn request_pass(headers: &HeaderMap, query: &str) -> String {
    if let Some(p) = headers
        .get("x-gallery-pass")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .filter(|p| !p.is_empty())
    {
        return p.to_string();
    }
    for part in query.split('&') {
        let (k, v) = match part.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        if k == "pass" {
            return String::from_utf8(percent_decode_bytes(v)).unwrap_or_default();
        }
    }
    String::new()
}

/// Cache key for a verified (stored hash, supplied password) pair: changing
/// the gallery password changes the hash, so old entries miss.
fn pass_cache_key(hash: &[u8], pass: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(hash);
    h.update([0u8]);
    h.update(pass.as_bytes());
    h.finalize().to_vec()
}

/// Slow Argon2 work runs on the blocking pool: never stall the runtime.
/// `None` = open (public gallery or correct password / owner bypass).
/// `Some(response)` = locked, return it directly.
async fn check_locked(
    state: &AppState,
    ip: IpAddr,
    slug: &str,
    pass: &str,
) -> Option<Response> {
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
        let key = pass_cache_key(&hash, pass);
        if let Some(at) = state.pass_ok.lock().unwrap().get(&key) {
            if at.elapsed() < PASS_CACHE_TTL {
                return None;
            }
        }
        if pass_fail_exceeded(state, &ip) {
            return Some(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "error": "too many attempts" })),
                )
                    .into_response(),
            );
        }
        // Bound Argon2 work: guessing must not exhaust the blocking pool.
        let _permit = state.argon2.acquire().await.ok();
        let ok = tokio::task::spawn_blocking({
            let pass = pass.to_string();
            move || crate::db::verify_password(&pass, &hash)
        })
        .await
        .unwrap_or(false);
        if ok {
            let mut cache = state.pass_ok.lock().unwrap();
            if cache.len() >= 10_000 {
                cache.retain(|_, at| at.elapsed() < PASS_CACHE_TTL);
            }
            cache.insert(key, std::time::Instant::now());
            return None;
        }
        pass_fail_record(state, &ip);
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

// --- gallery photo listing ---

pub async fn gallery_photos(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let t_total = Instant::now();
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if read_limited(&state, &ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many requests");
    }
    // No cache: plain scan is fast and never goes stale under concurrent upload.
    let t_db = Instant::now();
    let meta = {
        let db = state.db.lock().unwrap();
        db.gallery_meta(&slug)
    };
    tracing::debug!(stage = "list_db_check", latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0));
    let Some(meta) = meta else {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    };
    let key = request_key(&headers);
    let key_ok = key
        .as_ref()
        .map(|k| {
            let db = state.db.lock().unwrap();
            // Scoped: the key must own THIS slug, not just any gallery.
            db.key_owns_slug(k, &slug)
        })
        .unwrap_or(false);
    let owner = key_ok || owner_cookie_ok(&headers, &state, &slug);
    // Private gallery: nothing exists for anyone but the owner. A
    // supplied-but-wrong key is a guess: throttle it before the 404, so
    // private slugs cannot be probed at read-limit speed.
    if !meta.public && !owner {
        if key.is_some() && pass_fail_record(&state, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // Viewer lock: owner bypasses, otherwise require the password. A wrong
    // key with no pass was not counted yet, so count it once on denial
    // (check_locked already counted a non-empty wrong pass).
    if !owner {
        let query = raw.unwrap_or_default();
        let pass = request_pass(&headers, &query);
        if let Some(deny) = check_locked(&state, ip, &slug, &pass).await {
            if key.is_some() && pass.is_empty() && pass_fail_record(&state, &ip) {
                return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            }
            return deny;
        }
    }
    let t_scan = Instant::now();
    let result = photo_entries(&state, &slug).await;
    tracing::debug!(stage = "list_dir_scan", latency_ms = format!("{:.2}", t_scan.elapsed().as_secs_f64() * 1000.0));
    match result {
        Ok(photos) => {
            let count = photos.len();
            tracing::debug!(stage = "list_total", count = count, latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
            // The owner's Protect dialog needs this: an unauthenticated probe
            // cannot see the lock on a private gallery (it gets 404 first).
            let locked = {
                let db = state.db.lock().unwrap();
                db.gallery_pass_hash(&slug).is_some()
            };
            let body = Json(json!({ "slug": slug, "public": meta.public, "locked": locked, "photos": photos }));
            // Listings are dynamic + reveal lock state: never cache.
            // The owner's page needs the media cookie for <img> tags.
            if key_ok {
                let cookie = issue_owner_token(&state, &slug);
                return (
                    StatusCode::OK,
                    [
                        (header::CACHE_CONTROL, "private, no-store"),
                        (header::SET_COOKIE, cookie.as_str()),
                    ],
                    body,
                )
                    .into_response();
            }
            (StatusCode::OK, [(header::CACHE_CONTROL, "private, no-store")], body).into_response()
        }
        Err(_) => ApiError::Internal.into_response(),
    }
}

#[derive(Deserialize)]
struct VisibilityBody {
    public: bool,
}

/// Publish or unpublish the whole gallery. Owner key only.
pub async fn set_visibility(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    body: Body,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if rate_limited(&state, &ip) {
        return ApiError::TooMany.into_response();
    }
    let key = match request_key(&headers) {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let owned = {
        let db = state.db.lock().unwrap();
        db.key_owns_slug(&key, &slug)
    };
    if !owned {
        if pass_fail_record(&state, &ip) {
            return ApiError::TooMany.into_response();
        }
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let public = match serde_json::from_slice::<VisibilityBody>(&bytes) {
        Ok(b) => b.public,
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad request"),
    };
    let ok = {
        let mut db = state.db.lock().unwrap();
        db.set_public(&slug, public)
    };
    if !ok {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    (StatusCode::OK, Json(json!({ "slug": slug, "public": public }))).into_response()
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
        let Ok(md) = ent.metadata().await else { continue; };
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
    // Counters come from the DB: a directory scan cannot tell a banned
    // gallery's orphans from a live one, and it costs O(files).
    let rows: Vec<(String, i64, i64, i64, i64)> = {
        let db = state.db.lock().unwrap();
        match db.prepare("SELECT slug, created, photos, bytes, public FROM galleries ORDER BY created DESC") {
            Ok(mut stmt) => stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    let galleries: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(slug, created, photos, bytes, public)| {
            json!({
                "slug": slug,
                "created": created,
                "photos": photos,
                "bytes": bytes,
                "public": public != 0,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "galleries": galleries }))).into_response()
}

pub async fn admin_banned(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let rows: Vec<(i64, Option<String>)> = {
        let db = state.db.lock().unwrap();
        match db.prepare("SELECT created, label FROM banned_keys ORDER BY created DESC") {
            Ok(mut stmt) => stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    // The label is the banned key's first slug; the key hash itself is never
    // exposed (it is a credential digest).
    let banned: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(created, label)| json!({ "created": created, "label": label }))
        .collect();
    (StatusCode::OK, Json(json!({ "banned": banned }))).into_response()
}

/// Drop every banned token; the browser cookie is cleared in the response.
pub async fn admin_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    if let Some(tok) = crate::state::admin_token(&headers) {
        state.admin_tokens.lock().unwrap().remove(&tok);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            "admin=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
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
    // Own failure lockout: GALLY_RATE_MAX=0 must not open brute force.
    if pass_fail_exceeded(&state, &ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
    }
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();
    if admin_pass.is_empty() {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let guess: String = serde_json::from_slice::<UnlockBody>(&bytes)
        .map(|b| b.password)
        .unwrap_or_default();
    use subtle::ConstantTimeEq;
    let ok = guess.as_bytes().ct_eq(admin_pass.as_bytes()).unwrap_u8() == 1;
    if !ok {
        if pass_fail_record(&state, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
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
    // Always read the body: a header key plus a body slug must resolve, not
    // mint. Present-but-invalid slug is a client bug: reject, never mint.
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let parsed: Option<ResolveBody> = serde_json::from_slice(&bytes).ok();
    if key.is_none() {
        key = parsed.as_ref().and_then(|b| normalize_key(&b.key));
    }
    let mut slug_param: Option<String> = None;
    if let Some(b) = parsed.as_ref() {
        let s = b.slug.trim().to_string();
        if !s.is_empty() {
            if !is_slug(&s) {
                return err(StatusCode::BAD_REQUEST, "bad slug");
            }
            slug_param = Some(s);
        }
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
            false => {
                // Wrong key on an existing or missing slug answers identically
                // (403), but a guess still burns the failure bucket so probing
                // runs at 10/min, not at the generic limit (or unbounded when
                // GALLY_RATE_MAX=0). Correct keys resolve before this point,
                // so they can never be locked out.
                if pass_fail_record(&state, &ip) {
                    return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
                }
                err(StatusCode::FORBIDDEN, "forbidden")
            }
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
        [
            (header::CONTENT_TYPE, "text/html"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
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

pub async fn static_path(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let Some((bytes, ctype, etag)) = state.statics.get(uri.path()) else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    let resp = Response::builder()
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return resp
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap();
    }
    resp.header(header::CONTENT_TYPE, *ctype)
        .status(StatusCode::OK)
        .body(Body::from(*bytes))
        .unwrap()
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
        Some((bytes, ctype, _)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, *ctype), (header::CACHE_CONTROL, "no-cache")],
            Body::from(*bytes),
        )
            .into_response(),
        None => ApiError::Internal.into_response(),
    }
}

/// Delete every file belonging to `slug`. Returns
/// (removed, failed, still_remaining).
async fn sweep_files(state: &AppState, slug: &str) -> (usize, usize, bool) {
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
    (removed, failed, remaining)
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
    // The ban removes EVERY gallery of that key: their files must go too.
    let slugs = {
        let mut db = state.db.lock().unwrap();
        db.ban_gallery(&slug)
    };
    let Some(slugs) = slugs else {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    };
    let mut removed: usize = 0;
    let mut failed: usize = 0;
    let mut remaining = false;
    for s in &slugs {
        let (r, f, more) = sweep_files(&state, s).await;
        removed += r;
        failed += f;
        remaining |= more;
    }
    if failed > 0 || remaining {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "deleted": slug,
                "slugs": slugs,
                "files": removed,
                "error": "partial delete",
            })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "deleted": slug, "slugs": slugs, "files": removed })))
        .into_response()
}

/// Admin: delete ONE gallery row (and its files). The key stays valid.
pub async fn remove_gallery(
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
    let ok = {
        let mut db = state.db.lock().unwrap();
        db.remove_gallery(&slug)
    };
    if !ok {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let (removed, failed, remaining) = sweep_files(&state, &slug).await;
    if failed > 0 || remaining {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "deleted": slug, "files": removed, "error": "partial delete" })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "deleted": slug, "files": removed }))).into_response()
}

/// Admin: drop files whose gallery row is gone (crashes, old bans).
pub async fn admin_sweep(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let known: std::collections::HashSet<String> = {
        let db = state.db.lock().unwrap();
        match db.prepare("SELECT slug FROM galleries") {
            Ok(mut stmt) => stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    let mut deleted: usize = 0;
    let mut failed: usize = 0;
    let mut rd = match tokio::fs::read_dir(&state.dir).await {
        Ok(r) => r,
        Err(_) => return ApiError::Internal.into_response(),
    };
    // The directory listing is a snapshot: an upload mid-stream (.part-) or a
    // gallery created after the query keeps its files, so never delete those.
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Some(slug) = photo_slug(&name) else { continue };
        if known.contains(slug) {
            continue;
        }
        // Gallery may have been created after the snapshot above; re-check
        // before deleting so a fresh gallery's files are never swept.
        {
            let db = state.db.lock().unwrap();
            if db.gallery_exists(slug) {
                continue;
            }
        }
        if tokio::fs::remove_file(ent.path()).await.is_ok() {
            deleted += 1;
        } else {
            failed += 1;
        }
    }
    (StatusCode::OK, Json(json!({ "deleted": deleted, "failed": failed }))).into_response()
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
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    body: Body,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // Correct keys are checked before the failure bucket, so a valid key is
    // never locked out by someone else's guessing.
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    let key = match request_key(&headers) {
        Some(k) => k,
        None => {
            if pass_fail_record(&state, &ip) {
                return ApiError::TooMany.into_response();
            }
            return ApiError::Unauthorized.into_response();
        }
    };
    let known = {
        let db = state.db.lock().unwrap();
        db.key_owns_slug(&key, slug.as_str())
    };
    if !known {
        if pass_fail_record(&state, &ip) {
            return ApiError::TooMany.into_response();
        }
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    // Malformed JSON never silently clears the lock: reject it.
    let password: String = match serde_json::from_slice::<LockBody>(&bytes) {
        Ok(b) => b.password,
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad request"),
    };
    let hash = if password.is_empty() {
        None
    } else {
        // Argon2 is slow by design: hash off the async runtime, bounded.
        let _permit = state.argon2.acquire().await.ok();
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
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
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

/// Undoes a gallery mint that never received a byte: every early return in
/// the upload path (busy, full disk, bad body) would otherwise leave an empty
/// row behind. Disarmed once a file is stored.
struct MintGuard<'a> {
    state: &'a AppState,
    slug: String,
    keep: bool,
}

impl Drop for MintGuard<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Ok(mut db) = self.state.db.lock() {
            let _ = db.remove_gallery(&self.slug);
        }
    }
}

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
    let target = match request_slug(&headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let slug = {
        // Gallery page: key must own this slug. Unknown and not-yours answer
        // identically (404), like private listings, so a valid key cannot
        // enumerate private slugs. Banned keys stay banned.
        let db = state.db.lock().unwrap();
        match (&target, &key) {
            (Some(t), k) => {
                if db.is_banned(&key_hash(k)) {
                    return err(StatusCode::FORBIDDEN, "gallery banned");
                }
                if !(db.gallery_exists(t) && db.key_owns_slug(k, t)) {
                    drop(db);
                    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
                    if pass_fail_record(&state, &ip) {
                        return ApiError::TooMany.into_response();
                    }
                    return err(StatusCode::NOT_FOUND, "unknown gallery");
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
    // Minted here? Then the row exists only because of this request: drop it
    // again unless a file actually lands.
    let mut mint = target.is_none().then(|| MintGuard {
        state: &state,
        slug: slug.clone(),
        keep: false,
    });
    // Quota check before any bytes land: a full gallery costs a JSON reply.
    let limits = state.limits;
    let (used_bytes, used_photos) = {
        let db = state.db.lock().unwrap();
        let m = db.gallery_meta(&slug).unwrap_or(GalleryMeta { public: false, bytes: 0, photos: 0 });
        (m.bytes.max(0) as u64, m.photos)
    };
    if limits.gallery_bytes > 0 && used_bytes >= limits.gallery_bytes {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full");
    }
    if limits.gallery_photos > 0 && used_photos >= limits.gallery_photos {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "photo limit reached");
    }
    // Bounded concurrency: refuse outright instead of queueing disk writers.
    let _permit = match state.uploads.try_acquire() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::SERVICE_UNAVAILABLE, "busy"),
    };
    if let Some(free) = free_bytes(&state.dir) {
        if limits.min_free > 0 && free < limits.min_free {
            return err(StatusCode::INSUFFICIENT_STORAGE, "storage full");
        }
    }
    let (sanitized, _ext) = sanitize_filename(&raw_name);
    let t_body = Instant::now();
    // Stream into a dot-prefixed temp file: `photo_entries` skips dot names,
    // so a stalled or aborted upload is never listed or served as a photo.
    // The real name only appears after a successful `rename`.
    let uuid = uuid::Uuid::new_v4().as_simple().to_string();
    let temp_path = state.dir.join(format!(".part-{uuid}"));
    let final_name = format!("{slug}_{uuid}_{sanitized}");
    let mut out = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
    {
        Ok(f) => f,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store"),
    };
    let mut total: usize = 0;
    let started = Instant::now();
    let mut next_free_check = FREE_CHECK_EVERY;
    let mut stream = body.into_data_stream();
    loop {
        // Idle timeout: a dead socket must not pin an upload slot forever.
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                drop(out);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return err(StatusCode::REQUEST_TIMEOUT, "upload timed out");
            }
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => {
                drop(out);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return ApiError::TooLarge.into_response();
            }
        };
        if chunk.is_empty() {
            continue;
        }
        total += chunk.len();
        let ok = started.elapsed() <= UPLOAD_DEADLINE
            && total <= limits.max_file
            && (limits.gallery_bytes == 0
                || used_bytes + total as u64 <= limits.gallery_bytes);
        if !ok {
            drop(out);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return ApiError::TooLarge.into_response();
        }
        if total as u64 >= next_free_check {
            next_free_check = total as u64 + FREE_CHECK_EVERY;
            if let Some(free) = free_bytes(&state.dir) {
                if limits.min_free > 0 && free < limits.min_free {
                    drop(out);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return err(StatusCode::INSUFFICIENT_STORAGE, "storage full");
                }
            }
        }
        if out.write_all(&chunk).await.is_err() {
            drop(out);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return ApiError::Internal.into_response();
        }
    }
    drop(out);
    tracing::debug!(stage = "body_stream", bytes = total, latency_ms = format!("{:.2}", t_body.elapsed().as_secs_f64() * 1000.0));
    if total == 0 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return err(StatusCode::BAD_REQUEST, "empty body");
    }
    if tokio::fs::rename(&temp_path, state.dir.join(&final_name))
        .await
        .is_err()
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return ApiError::Internal.into_response();
    }
    // Counters are the quota truth: reconcile after the file exists so a
    // concurrent overshoot is corrected by deleting this file again.
    let usage = {
        let mut db = state.db.lock().unwrap();
        db.add_upload(&slug, total as u64)
    };
    if let Some((bytes, photos)) = usage {
        let over = (limits.gallery_bytes > 0 && bytes as u64 > limits.gallery_bytes)
            || (limits.gallery_photos > 0 && photos > limits.gallery_photos);
        if over {
            let _ = tokio::fs::remove_file(state.dir.join(&final_name)).await;
            let mut db = state.db.lock().unwrap();
            let _ = db.remove_upload(&slug, total as u64);
            return err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full");
        }
    } else {
        // Row vanished mid-upload (ban/reset raced us): drop the orphan file.
        let _ = tokio::fs::remove_file(state.dir.join(&final_name)).await;
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    tracing::debug!(stage = "put_total", latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0));
    if let Some(m) = &mut mint {
        m.keep = true;
    }
    (StatusCode::CREATED, Json(json!({ "name": final_name, "slug": slug }))).into_response()
}
// --- photo serve / delete ---

pub async fn get_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let t_total = Instant::now();
    // Orphaned/banned files stay on disk but are unreachable: reject names
    // outside a known gallery before touching the filesystem.
    let slug = match photo_slug(&name) {
        Some(s) => s.to_string(),
        None => return err(StatusCode::NOT_FOUND, "not found"),
    };
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    if read_limited(&state, &ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many requests");
    }
    let meta = {
        let db = state.db.lock().unwrap();
        db.gallery_meta(&slug)
    };
    let Some(meta) = meta else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    // ?pass= in <img> URLs stays: media tags cannot set headers, so the
    // query secret is the only way to authorize direct photo embeds.
    let query = raw.unwrap_or_default();
    let pass = request_pass(&headers, &query);
    let key = request_key(&headers);
    let key_ok = key
        .as_ref()
        .map(|k| {
            let db = state.db.lock().unwrap();
            db.key_owns_slug(k, &slug)
        })
        .unwrap_or(false);
    let owner = key_ok || owner_cookie_ok(&headers, &state, &slug);
    // A supplied-but-wrong key on a private gallery is a guess: throttle it
    // before the 404, like the private listing, so private photos cannot be
    // probed at read-limit speed. No key (public probing) burns nothing.
    if !meta.public && !owner && key.is_some() {
        if pass_fail_record(&state, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
    }
    // Private gallery: photos are invisible without the key or its cookie.
    if !meta.public && !owner {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    // Viewer lock applies per-file too (direct /photos/ links). A wrong key
    // with no pass was not counted yet, so count it once on denial
    // (check_locked already counted a non-empty wrong pass).
    if !owner {
        if let Some(deny) = check_locked(&state, ip, &slug, &pass).await {
            if key.is_some() && pass.is_empty() && pass_fail_record(&state, &ip) {
                return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            }
            return deny;
        }
    }
    // Anything a non-owner can reach here is either private (never cache) or
    // a public gallery's public bytes. Owner-key requests always stay private.
    // A locked gallery (pass_hash set) or a supplied viewer password means
    // the bytes are access-controlled: never mark them public for caches.
    let locked = {
        let db = state.db.lock().unwrap();
        db.gallery_pass_hash(&slug).is_some()
    };
    let cache_control = if meta.public && !owner && pass.is_empty() && !locked {
        "public, max-age=31536000, immutable"
    } else {
        "private, no-store"
    };
    let t_canon = Instant::now();
    let path = state.dir.join(&name);
    // Contain within photos dir (no symlink escape); base canonicalized once.
    let canon_file = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "not found"),
    };
    if !canon_file.starts_with(&state.canon_dir) {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let mut file = match tokio::fs::File::open(&canon_file).await {
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
    // SVG runs in site origin: sandbox it and force download. Anything else
    // that browsers cannot render inline is a download, never a sniffed page.
    let inline = ctype.starts_with("image/")
        || ctype.starts_with("video/")
        || ctype.starts_with("audio/")
        || ctype.starts_with("text/");
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ACCEPT_RANGES, "bytes")
        .header("x-content-type-options", "nosniff");
    if ext == ".svg" {
        builder = builder
            .header(header::CONTENT_SECURITY_POLICY, "sandbox")
            .header(header::CONTENT_DISPOSITION, "attachment");
    } else if !inline {
        builder = builder.header(header::CONTENT_DISPOSITION, "attachment");
    }
    match parse_range(&headers, len) {
        // Unsatisfiable: 416 with the real length, no body (RFC 9110 §15.5.17).
        Some(Err(())) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap(),
        Some(Ok((start, end))) => {
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
            }
            let take = file.take(end - start + 1);
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, (end - start + 1).to_string())
                .body(Body::from_stream(tokio_util::io::ReaderStream::new(take)))
                .unwrap()
        }
        None => builder
            .header(header::CONTENT_LENGTH, len.to_string())
            .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
            .unwrap(),
    }
}

pub async fn delete_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
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
    if !is_admin(&headers, &state) {
        let key = request_key(&headers);
        let ok = match key {
            Some(k) => {
                let db = state.db.lock().unwrap();
                db.gallery_exists(&slug) && db.key_owns_slug(&k, slug.as_str())
            }
            None => false,
        };
        if !ok {
            // Unknown and not-yours answer identically (404), like private
            // listings, so a valid key cannot enumerate private slugs.
            // Correct keys are checked first, so they can never be locked out.
            let ip = client_ip(&headers, Some(ConnectInfo(addr)));
            if pass_fail_record(&state, &ip) {
                return ApiError::TooMany.into_response();
            }
            return err(StatusCode::NOT_FOUND, "not found");
        }
    }
    let path = state.dir.join(&name);
    match path.canonicalize() {
        Ok(canon) if canon.starts_with(&state.canon_dir) => {}
        _ => return err(StatusCode::NOT_FOUND, "not found"),
    }
    // Size before the file vanishes: counters keep the gallery quota honest.
    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    match tokio::fs::remove_file(&path).await {
        // Keep the gallery row: a concurrent upload landing between a file
        // count and a row delete orphaned files under a dead slug.
        Ok(()) => {
            let mut db = state.db.lock().unwrap();
            let _ = db.remove_upload(&slug, size);
            drop(db);
            (StatusCode::OK, Json(json!({ "deleted": name, "gallery_gone": false }))).into_response()
        }
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
