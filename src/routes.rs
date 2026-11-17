use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::db::{GalleryDb, GalleryMeta};
use crate::state::{
    AppState, MediaToken, OWNER_TOKEN_TTL, PASS_CACHE_TTL, is_admin, pass_fail_exceeded,
    pass_fail_record, rate_limited, read_limited,
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
/// Abandoned resumable-upload temps count against the gallery quota until
/// this age, then a sweep reclaims them.
const PART_TTL: Duration = Duration::from_secs(3600);

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

/// fsync a directory so a rename/remove survives a power cut. Some
/// filesystems reject directory fsync; that is best effort.
async fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || match std::fs::File::open(&dir)?.sync_all() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(std::io::Error::other)?
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Cookie name for one kind of media token: owner and viewer never share one.
fn media_cookie(slug: &str, kind: MediaToken) -> String {
    let prefix = match kind {
        MediaToken::Owner => "galowner",
        MediaToken::Viewer => "galpass",
    };
    format!("{prefix}_{slug}")
}

/// True when the request carries a live media cookie of `kind` for `slug`.
fn media_cookie_ok(headers: &HeaderMap, state: &AppState, slug: &str, kind: MediaToken) -> bool {
    let Some(tok) = crate::state::cookie_value(headers, &media_cookie(slug, kind)) else {
        return false;
    };
    let version = match kind {
        // Owner cookies are not revocable per gallery: only the key mints them.
        MediaToken::Owner => 0,
        MediaToken::Viewer => match state.db.lock().unwrap().gallery_token_version(slug) {
            Some(v) => v,
            None => return false,
        },
    };
    crate::state::media_token_ok(&state.media_secret, tok, slug, kind, version)
}

/// Mint a media cookie for `slug` (for `<img>`/`<video>`, which cannot send
/// the key or password header). Returns the full `Set-Cookie` value.
fn issue_media_token(state: &AppState, slug: &str, kind: MediaToken, version: i64) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + OWNER_TOKEN_TTL.as_secs();
    let tok = crate::state::media_token(&state.media_secret, slug, kind, version, exp);
    // `Secure` only when the operator says the front end is HTTPS.
    let secure = if state.limits.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={tok}; HttpOnly; SameSite=Lax; Path=/photos/; Max-Age={}{secure}",
        media_cookie(slug, kind),
        OWNER_TOKEN_TTL.as_secs()
    )
}

/// Bounded, timeout-guarded JSON body read.
// Response is the cold error path; boxing it would only add an allocation.
#[allow(clippy::result_large_err)]
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

/// True when `If-None-Match` names `etag` (or `*`).
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',').any(|t| {
                let t = t.trim();
                t == "*" || t == etag
            })
        })
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

/// Preview edge and JPEG quality (one small file per photo).
const THUMB_EDGE: u32 = 400;
const THUMB_QUALITY: u8 = 80;
/// Decompression-bomb guard: refuse absurd dimensions before decoding.
const THUMB_MAX_PIXELS: u64 = 50_000_000;
const THUMB_MAX_SIDE: u32 = 16_384;

fn thumb_supported(ext: &str) -> bool {
    matches!(
        ext,
        ".jpg" | ".jpeg" | ".png" | ".gif" | ".webp" | ".bmp" | ".ico"
    )
}

/// Write `<photos>/.thumbs/<name>.jpg`, best effort. Old photos may have no
/// thumb: the client falls back to the original. Add a backfill CLI only if
/// existing libraries need previews.
async fn write_thumb(state: &AppState, src: &std::path::Path, name: &str) {
    let dir = state.dir.join(".thumbs");
    let dest = dir.join(format!("{name}.jpg"));
    // Write-then-rename: a concurrent GET must never see a half-written JPEG
    // (a public gallery would cache that for a day).
    let temp = dir.join(format!(".tmp-{}.jpg", uuid::Uuid::new_v4().as_simple()));
    let src = src.to_path_buf();
    // Two decodes at a time: even a small result needs the source bitmap
    // (width*height*4 bytes) in memory while it is resized.
    let _permit = state.thumbs.acquire().await.ok();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let out = (|| {
            let (w, h) = image::ImageReader::open(&src)?
                .with_guessed_format()?
                .into_dimensions()?;
            if w > THUMB_MAX_SIDE
                || h > THUMB_MAX_SIDE
                || u64::from(w) * u64::from(h) > THUMB_MAX_PIXELS
            {
                anyhow::bail!("image too large");
            }
            let img = image::ImageReader::open(&src)?
                .with_guessed_format()?
                .decode()?;
            let thumb = img.thumbnail(THUMB_EDGE, THUMB_EDGE);
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = std::fs::File::create(&temp)?;
            thumb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                file,
                THUMB_QUALITY,
            ))?;
            std::fs::rename(&temp, &dest)?;
            Ok(())
        })();
        // Derived file: no fsync, but never leave a partial temp behind.
        if out.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        out
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(thumb = name, error = %e, "thumbnail skipped"),
        Err(e) => tracing::debug!(thumb = name, error = %e, "thumbnail task failed"),
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

/// Liveness/readiness probe: DB reachable, photos dir readable, and free
/// space above `GALLY_MIN_FREE_BYTES` (0 = ignore).
pub async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let db_ok = {
        let db = state.db.lock().unwrap();
        db.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok()
    };
    let dir_ok = tokio::fs::read_dir(&state.dir).await.is_ok();
    let _ = sweep_and_measure_parts(&state, None, None).await;
    let free = free_bytes(&state.dir);
    let free_ok = state.limits.min_free == 0 || free.is_some_and(|f| f >= state.limits.min_free);
    let ok = db_ok && dir_ok && free_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "ok": ok, "free_bytes": free.unwrap_or(0) })),
    )
        .into_response()
}

/// Prometheus scrape. Loopback only (honours `GALLY_TRUST_PROXY`, so a
/// trusted reverse proxy can resolve the real client IP): never public.
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
    let local = ip.is_loopback() || metrics_direct_peer(addr.ip(), &headers, &TRUSTED_PROXIES);
    if !local {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let (galleries, photos, bytes) = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(photos),0), COALESCE(SUM(bytes),0) FROM galleries",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap_or((0, 0, 0))
    };
    let load = std::sync::atomic::Ordering::Relaxed;
    let body = format!(
        "# TYPE gally_uptime_seconds gauge\n\
         gally_uptime_seconds {}\n\
         # TYPE gally_free_bytes gauge\n\
         gally_free_bytes {}\n\
         # TYPE gally_galleries gauge\n\
         gally_galleries {galleries}\n\
         # TYPE gally_photos gauge\n\
         gally_photos {photos}\n\
         # TYPE gally_bytes gauge\n\
         gally_bytes {bytes}\n\
         # TYPE gally_requests_total counter\n\
         gally_requests_total {}\n\
         # TYPE gally_responses_5xx_total counter\n\
         gally_responses_5xx_total {}\n\
         # TYPE gally_uploads_total counter\n\
         gally_uploads_total {}\n\
         # TYPE gally_upload_bytes_total counter\n\
         gally_upload_bytes_total {}\n",
        state.started.elapsed().as_secs(),
        free_bytes(&state.dir).unwrap_or(0),
        state.requests_total.load(load),
        state.responses_5xx_total.load(load),
        state.uploads_total.load(load),
        state.upload_bytes_total.load(load),
    );
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `GALLY_TRUST_PROXY`: comma-separated IPs/CIDRs allowed to set
/// `X-Forwarded-For`. `1/true/yes` keeps the old meaning: loopback only.
/// Parsed once; changing it needs a restart.
static TRUSTED_PROXIES: std::sync::LazyLock<Vec<ipnet::IpNet>> = std::sync::LazyLock::new(|| {
    let raw = std::env::var("GALLY_TRUST_PROXY").unwrap_or_default();
    parse_trust_proxy(&raw)
});

fn parse_trust_proxy(raw: &str) -> Vec<ipnet::IpNet> {
    let mut nets = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if matches!(token.to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
            nets.push("127.0.0.0/8".parse().expect("valid loopback net"));
            nets.push("::1/128".parse().expect("valid loopback net"));
        } else if let Ok(net) = token.parse::<ipnet::IpNet>() {
            nets.push(net);
        } else if let Ok(addr) = token.parse::<IpAddr>() {
            nets.push(ipnet::IpNet::from(addr));
        } else {
            tracing::warn!(value = token, "GALLY_TRUST_PROXY: ignoring invalid IP/CIDR");
        }
    }
    nets
}

/// A direct (not proxied) connection from a trusted peer is local for
/// `/metrics`: in a container the host reaches the app through the Docker
/// bridge gateway, not loopback. A forwarded request is only local if the
/// resolved client is loopback, so public `/metrics` stays 404.
fn metrics_direct_peer(peer: IpAddr, headers: &HeaderMap, trusted: &[ipnet::IpNet]) -> bool {
    !headers.contains_key("x-forwarded-for") && trusted.iter().any(|net| net.contains(&peer))
}

pub(crate) fn client_ip(
    headers: &HeaderMap,
    info: Option<ConnectInfo<std::net::SocketAddr>>,
) -> IpAddr {
    client_ip_from(headers, info, &TRUSTED_PROXIES)
}

fn client_ip_from(
    headers: &HeaderMap,
    info: Option<ConnectInfo<std::net::SocketAddr>>,
    trusted: &[ipnet::IpNet],
) -> IpAddr {
    // Never trust X-Forwarded-For by default: any client can spoof it and
    // dodge the rate limit. Only the real connection address counts, unless
    // the peer is in GALLY_TRUST_PROXY (a reverse proxy, local or remote).
    // Then walk XFF right-to-left past trusted proxies: the first address
    // not in the list is the client, so every proxy hop must be listed.
    let peer = info.map(|c| c.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    if !trusted.iter().any(|net| net.contains(&peer)) {
        return peer;
    }
    let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) else {
        return peer;
    };
    let mut leftmost = None;
    for hop in xff.rsplit(',').map(str::trim) {
        let Ok(ip) = hop.parse::<IpAddr>() else {
            // A malformed hop makes the whole chain unverifiable: fall back
            // to the peer rather than guess a spoofable address.
            return peer;
        };
        if !trusted.iter().any(|net| net.contains(&ip)) {
            return ip;
        }
        leftmost = Some(ip);
    }
    leftmost.unwrap_or(peer)
}

fn request_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-gallery-key")
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_key)
}

/// Target gallery for uploads. Absent on home (= mint a fresh gallery).
/// Present-but-invalid is a client bug: reject, never silently mint.
// Same as read_body: the error response is a cold path.
#[allow(clippy::result_large_err)]
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
    if let Some(v) = headers.get("x-gallery-pass") {
        // Header values are ASCII-only on the way out of fetch(), so app.js
        // percent-encodes: decode here (raw old-style values decode to
        // themselves when they hold no `%XX`).
        if let Some(p) = std::str::from_utf8(v.as_bytes())
            .ok()
            .map(|p| String::from_utf8(percent_decode_bytes(p)).unwrap_or_default())
            .filter(|p| !p.is_empty())
        {
            return p;
        }
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

/// First `name=` value in a raw query string, percent-decoded.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k == name)
            .then(|| String::from_utf8(percent_decode_bytes(v)).ok())
            .flatten()
    })
}

/// Listing cursor: `<created_ms>:<name>` (split at the first colon).
fn parse_cursor(s: &str) -> Option<(i64, String)> {
    let (created, name) = s.split_once(':')?;
    let created = created.parse::<i64>().ok()?;
    (!name.is_empty()).then(|| (created, name.to_string()))
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

/// Result of the viewer lock check: the caller needs to know not just
/// "refused" but whether a password was proven, so it can set the cookie.
enum Locked {
    /// No viewer password set on the gallery.
    Open,
    /// Password proven just now (or by the pass cache): set the cookie.
    Unlocked,
    /// Locked and unproven: return this response as-is.
    Denied(Response),
}

/// Slow Argon2 work runs on the blocking pool: never stall the runtime.
async fn check_locked(state: &AppState, ip: IpAddr, slug: &str, pass: &str) -> Locked {
    let stored = {
        let db = state.db.lock().unwrap();
        db.gallery_pass_hash(slug)
    };
    let Some(hash) = stored else {
        return Locked::Open;
    };
    if !pass.is_empty() {
        // The owner key also works in the viewer-password field (the dialog
        // promises it): it is the strongest credential for this gallery.
        let owns = {
            let db = state.db.lock().unwrap();
            db.key_owns_slug(pass, slug)
        };
        if owns {
            return Locked::Unlocked;
        }
        let key = pass_cache_key(&hash, pass);
        if let Some(at) = state.pass_ok.lock().unwrap().get(&key)
            && at.elapsed() < PASS_CACHE_TTL
        {
            return Locked::Unlocked;
        }
        if pass_fail_exceeded(state, &ip) {
            return Locked::Denied(
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
            return Locked::Unlocked;
        }
        pass_fail_record(state, &ip);
    }
    Locked::Denied(
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "gallery locked", "locked": true })),
        )
            .into_response(),
    )
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
    tracing::debug!(
        stage = "list_db_check",
        latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0)
    );
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
    let owner = key_ok || media_cookie_ok(&headers, &state, &slug, MediaToken::Owner);
    // Private gallery: nothing exists for anyone but the owner. A
    // supplied-but-wrong key is a guess: throttle it before the 404, so
    // private slugs cannot be probed at read-limit speed.
    if !meta.public && !owner {
        if key.is_some() && pass_fail_record(&state, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    let query = raw.unwrap_or_default();
    let limit = query_param(&query, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(1, 1000);
    let before = match query_param(&query, "before") {
        None => None,
        Some(c) => match parse_cursor(&c) {
            Some(b) => Some(b),
            None => return err(StatusCode::BAD_REQUEST, "bad cursor"),
        },
    };
    // Viewer lock: owner bypasses; a live viewer cookie for THIS gallery is
    // the password, already proven (no Argon2, no URL secret). A wrong key
    // with no pass was not counted yet, so count it once on denial
    // (check_locked already counted a non-empty wrong pass).
    let mut unlocked = false;
    if !owner {
        let viewer = media_cookie_ok(&headers, &state, &slug, MediaToken::Viewer);
        if !viewer {
            let pass = request_pass(&headers, &query);
            match check_locked(&state, ip, &slug, &pass).await {
                Locked::Open => {}
                Locked::Unlocked => unlocked = true,
                Locked::Denied(deny) => {
                    if key.is_some() && pass.is_empty() && pass_fail_record(&state, &ip) {
                        return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
                    }
                    return deny;
                }
            }
        }
    }
    let t_scan = Instant::now();
    // One query for the page plus the lock/cookie state.
    let (page, locked, version) = {
        let db = state.db.lock().unwrap();
        (
            db.photos_page(&slug, before, limit),
            // The owner's Protect dialog needs this: an unauthenticated probe
            // cannot see the lock on a private gallery (it gets 404 first).
            db.gallery_pass_hash(&slug).is_some(),
            db.gallery_token_version(&slug).unwrap_or(0),
        )
    };
    // A DB failure must not masquerade as an empty gallery.
    let rows = match page {
        Ok(rows) => rows,
        Err(_) => return ApiError::Internal.into_response(),
    };
    tracing::debug!(
        stage = "list_db_page",
        count = rows.len(),
        latency_ms = format!("{:.2}", t_scan.elapsed().as_secs_f64() * 1000.0)
    );
    // A full page means more may follow: the last row is the next cursor.
    let next = if rows.len() == limit {
        rows.last()
            .map(|(created, name)| format!("{created}:{name}"))
    } else {
        None
    };
    let photos: Vec<serde_json::Value> = rows
        .iter()
        .map(|(_, name)| json!({ "name": name }))
        .collect();
    tracing::debug!(
        stage = "list_total",
        count = photos.len(),
        latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0)
    );
    let body = Json(json!({
        "slug": slug,
        "public": meta.public,
        "locked": locked,
        "photos": photos,
        "next": next,
    }));
    // Listings are dynamic + reveal lock state: never cache.
    // <img> tags cannot send a header: the owner's page needs the
    // owner media cookie, a fresh unlock needs the viewer one.
    let mut resp = (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "private, no-store")],
        body,
    )
        .into_response();
    let cookie = if key_ok {
        Some(issue_media_token(&state, &slug, MediaToken::Owner, 0))
    } else if unlocked {
        Some(issue_media_token(
            &state,
            &slug,
            MediaToken::Viewer,
            version,
        ))
    } else {
        None
    };
    if let Some(cookie) = cookie
        && let Ok(v) = HeaderValue::from_str(&cookie)
    {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
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
    (
        StatusCode::OK,
        Json(json!({ "slug": slug, "public": public })),
    )
        .into_response()
}

// --- admin ---

pub async fn admin_galleries(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    // Counters come from the DB: a directory scan costs O(files).
    let rows: Vec<(String, i64, i64, i64, i64)> = {
        let db = state.db.lock().unwrap();
        match db.prepare(
            "SELECT slug, created, photos, bytes, public FROM galleries ORDER BY created DESC",
        ) {
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

/// Drop the admin token; the browser cookie is cleared in the response.
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
    // `Secure` only when the operator serves the site over HTTPS; on plain
    // HTTP browsers would drop the cookie and break login.
    let secure = if state.limits.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!("admin={tok}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400{secure}");
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
    if !invite_ok(&state, &headers) {
        return err(StatusCode::FORBIDDEN, "invite required");
    }
    let slug = {
        let mut db = state.db.lock().unwrap();
        db.get_or_create_gallery(&key)
    };
    match slug {
        Some(s) => (StatusCode::OK, Json(json!({ "slug": s }))).into_response(),
        None => ApiError::Internal.into_response(),
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
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
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
        return (StatusCode::FOUND, [(header::LOCATION, "/")], "").into_response();
    }
    match state.statics.get("/index.html") {
        Some((bytes, ctype, _)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, *ctype),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from(*bytes),
        )
            .into_response(),
        None => ApiError::Internal.into_response(),
    }
}

/// Reap abandoned `.part-*` uploads older than `PART_TTL` and return how many
/// bytes of `slug` the live ones hold. `exclude` is the current request's own
/// temp: its committed prefix is already accounted for by the caller.
async fn sweep_and_measure_parts(
    state: &AppState,
    slug: Option<&str>,
    exclude: Option<&std::path::Path>,
) -> u64 {
    let Ok(mut rd) = tokio::fs::read_dir(&state.dir).await else {
        return 0;
    };
    let mut pending = 0u64;
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(".part-") else {
            continue;
        };
        // `.part-{slug}_{upload-id}`: neither part contains a stray underscore.
        let id = rest.rsplit('_').next().unwrap_or(rest);
        let active = state.upload_ids.lock().unwrap().contains(id);
        let Ok(md) = ent.metadata().await else {
            continue;
        };
        let expired = !active
            && md
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= PART_TTL);
        // A failed removal keeps the bytes on disk, so they keep counting.
        if expired && tokio::fs::remove_file(ent.path()).await.is_ok() {
            continue;
        }
        if exclude.is_some_and(|ex| ex == ent.path().as_path()) {
            continue;
        }
        if let Some(s) = slug
            && name.starts_with(&format!(".part-{s}_"))
        {
            pending = pending.saturating_add(md.len());
        }
    }
    pending
}

/// Remove `prefix*` files directly under `dir`; a missing dir is empty.
async fn sweep_dir(dir: &std::path::Path, prefix: &str) -> (usize, usize) {
    let mut removed = 0;
    let mut failed = 0;
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => loop {
            match rd.next_entry().await {
                Ok(Some(ent)) => {
                    if !ent.file_name().to_string_lossy().starts_with(prefix) {
                        continue;
                    }
                    if tokio::fs::remove_file(ent.path()).await.is_ok() {
                        removed += 1;
                    } else {
                        failed += 1;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    failed += 1;
                    break;
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => failed += 1,
    }
    (removed, failed)
}

/// Any `prefix*` file left under `dir`? A missing dir has none.
async fn has_prefixed(dir: &std::path::Path, prefix: &str) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => loop {
            match rd.next_entry().await {
                Ok(Some(ent)) => {
                    if ent.file_name().to_string_lossy().starts_with(prefix) {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return true,
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Delete every file belonging to `slug`, originals and thumbs. Returns
/// (removed, failed, still_remaining).
async fn sweep_files(state: &AppState, slug: &str) -> (usize, usize, bool) {
    let prefix = format!("{slug}_");
    let part_prefix = format!(".part-{slug}_");
    let thumbs = state.dir.join(".thumbs");
    let (mut removed, mut failed) = sweep_dir(&state.dir, &prefix).await;
    let (part_removed, part_failed) = sweep_dir(&state.dir, &part_prefix).await;
    removed += part_removed;
    failed += part_failed;
    let (thumb_removed, thumb_failed) = sweep_dir(&thumbs, &prefix).await;
    removed += thumb_removed;
    failed += thumb_failed;
    // A concurrent upload may land mid-sweep: re-scan, never claim success
    // while files remain. A scan error means we cannot prove the dirs are clean.
    let remaining = has_prefixed(&state.dir, &prefix).await
        || has_prefixed(&state.dir, &part_prefix).await
        || has_prefixed(&thumbs, &prefix).await;
    (removed, failed, remaining)
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
    // Preflight before touching the row: if the dir cannot be read the files
    // cannot be swept, so keep both the row and its files untouched.
    if tokio::fs::read_dir(&state.dir).await.is_err() {
        return ApiError::Internal.into_response();
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
    (
        StatusCode::OK,
        Json(json!({ "deleted": slug, "files": removed })),
    )
        .into_response()
}

/// Owner: delete ONE gallery row (and its files). Requires the gallery key.
pub async fn delete_own_gallery(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if !is_slug(&slug) {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    if !is_admin(&headers, &state) {
        let key = request_key(&headers);
        let ok = match key {
            Some(k) => {
                let db = state.db.lock().unwrap();
                db.gallery_exists(&slug) && db.key_owns_slug(&k, &slug)
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
    // Preflight before touching the row: if the dir cannot be read the files
    // cannot be swept, so keep both the row and its files untouched.
    if tokio::fs::read_dir(&state.dir).await.is_err() {
        return ApiError::Internal.into_response();
    }
    let ok = {
        let mut db = state.db.lock().unwrap();
        db.remove_gallery(&slug)
    };
    if !ok {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let (removed, failed, remaining) = sweep_files(&state, &slug).await;
    if failed > 0 || remaining {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "deleted": slug, "files": removed, "error": "partial delete" })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({ "deleted": slug, "files": removed })),
    )
        .into_response()
}

/// Delete files under `dir` whose gallery is gone. `known` is the snapshot;
/// each hit is re-checked live so a gallery created after the snapshot keeps
/// its files. `required` = the directory must exist (photos), while a missing
/// `.thumbs` on an old install is fine.
async fn sweep_orphans(
    state: &AppState,
    dir: &std::path::Path,
    known: &std::collections::HashSet<String>,
    required: bool,
    deleted: &mut usize,
    failed: &mut usize,
) -> Result<(), ()> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(()),
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        // Abandoned resumable-upload temp: `.part-{slug}_{upload-id}`.
        if let Some(rest) = name.strip_prefix(".part-") {
            let Some((slug, _)) = rest.split_once('_') else {
                continue;
            };
            let id = rest.rsplit('_').next().unwrap_or(rest);
            if !is_slug(slug) || state.upload_ids.lock().unwrap().contains(id) {
                continue;
            }
            let exists = {
                let db = state.db.lock().unwrap();
                db.gallery_exists(slug)
            };
            if exists {
                continue;
            }
            if tokio::fs::remove_file(ent.path()).await.is_ok() {
                *deleted += 1;
            } else {
                *failed += 1;
            }
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let Some(slug) = photo_slug(&name) else {
            continue;
        };
        if known.contains(slug) {
            continue;
        }
        {
            let db = state.db.lock().unwrap();
            if db.gallery_exists(slug) {
                continue;
            }
        }
        if tokio::fs::remove_file(ent.path()).await.is_ok() {
            *deleted += 1;
        } else {
            *failed += 1;
        }
    }
    Ok(())
}

/// Admin: drop files whose gallery row is gone (e.g. crashes).
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
    // Reap expired temps first; the orphan pass below handles the rest.
    let _ = sweep_and_measure_parts(&state, None, None).await;
    // The directory listing is a snapshot: an upload mid-stream (.part-) or a
    // gallery created after the query keeps its files, so never delete those.
    if sweep_orphans(&state, &state.dir, &known, true, &mut deleted, &mut failed)
        .await
        .is_err()
    {
        return ApiError::Internal.into_response();
    }
    // Thumbs mirror their originals; a missing dir on old installs is fine.
    let _ = sweep_orphans(
        &state,
        &state.dir.join(".thumbs"),
        &known,
        false,
        &mut deleted,
        &mut failed,
    )
    .await;
    (
        StatusCode::OK,
        Json(json!({ "deleted": deleted, "failed": failed })),
    )
        .into_response()
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
    if rate_limited(&state, &ip) {
        return ApiError::TooMany.into_response();
    }
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
    // Old unlocks die with the old password: set_gallery_pass bumped
    // token_version, so every viewer cookie minted before this is invalid.
    (
        StatusCode::OK,
        Json(json!({ "slug": slug, "locked": hash.is_some() })),
    )
        .into_response()
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

/// One resumable-upload chunk: `END` is inclusive, `TOTAL` is the file size.
#[derive(Clone, Copy)]
struct UploadChunk {
    start: u64,
    end: u64,
    total: u64,
}

/// Parse exactly `bytes START-END/TOTAL`; `None` for `*`, inverted or
/// overflowing ranges, and empty files.
fn parse_content_range(raw: &str) -> Option<UploadChunk> {
    let rest = raw.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    let total: u64 = total.parse().ok()?;
    if start > end || end >= total || total == 0 {
        return None;
    }
    Some(UploadChunk { start, end, total })
}

/// Exactly 32 ASCII hex digits, normalized to lowercase.
fn parse_upload_id(raw: &str) -> Option<String> {
    let id = raw.to_ascii_lowercase();
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// Both resumable-upload headers, valid; or neither (legacy full-body PUT).
// Response is the cold error path; boxing it would only add an allocation.
#[allow(clippy::result_large_err)]
fn parse_upload_headers(headers: &HeaderMap) -> Result<Option<(UploadChunk, String)>, Response> {
    let (range, id) = (headers.get("content-range"), headers.get("x-upload-id"));
    match (range, id) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(err(
            StatusCode::BAD_REQUEST,
            "content-range and x-upload-id go together",
        )),
        (Some(range), Some(id)) => {
            let id = id
                .to_str()
                .ok()
                .and_then(parse_upload_id)
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad upload id"))?;
            let chunk = range
                .to_str()
                .ok()
                .and_then(parse_content_range)
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad content-range"))?;
            Ok(Some((chunk, id)))
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
    let chunk = match parse_upload_headers(&headers) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    // One file arrives as many requests: only its first chunk (or a legacy
    // full-body PUT) burns a rate-limit hit, or chunking trips the limit.
    let first_or_legacy = match &chunk {
        None => true,
        Some((c, _)) => c.start == 0,
    };
    if first_or_legacy && rate_limited(&state, &ip) {
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
    // The browser mints the gallery first; a chunked request without a slug
    // would mint a new gallery per chunk.
    if chunk.is_some() && target.is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "gallery slug required for resumable upload",
        );
    }
    let slug = {
        // Gallery page: key must own this slug. Unknown and not-yours answer
        // identically (404), like private listings, so a valid key cannot
        // enumerate private slugs.
        let db = state.db.lock().unwrap();
        match (&target, &key) {
            (Some(t), k) => {
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
                if !invite_ok(&state, &headers) {
                    return err(StatusCode::FORBIDDEN, "invite required");
                }
                drop(db);
                let mut db = state.db.lock().unwrap();
                match db.get_or_create_gallery(k) {
                    Some(s) => s,
                    None => return ApiError::Internal.into_response(),
                }
            }
        }
    };
    tracing::debug!(
        stage = "db_get_or_create",
        latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0)
    );
    // Minted here? Then the row exists only because of this request: drop it
    // again unless a file actually lands.
    let mut mint = target.is_none().then(|| MintGuard {
        state: &state,
        slug: slug.clone(),
        keep: false,
    });
    let (sanitized, ext) = sanitize_filename(&raw_name);
    // Resumable uploads name the final file after the client's upload id, so
    // every retried chunk targets the same file; legacy keeps a random uuid.
    let uuid = uuid::Uuid::new_v4().as_simple().to_string();
    let (final_name, temp_path) = match &chunk {
        Some((_, id)) => (
            format!("{slug}_{id}_{sanitized}"),
            state.dir.join(format!(".part-{slug}_{id}")),
        ),
        None => (
            format!("{slug}_{uuid}_{sanitized}"),
            state.dir.join(format!(".part-{slug}_{uuid}")),
        ),
    };
    // Lost reply to the final chunk: the file is already stored, answer
    // success without reading the body again.
    if chunk.is_some()
        && tokio::fs::metadata(state.dir.join(&final_name))
            .await
            .is_ok()
        && state.db.lock().unwrap().photo_exists(&slug, &final_name)
    {
        return (
            StatusCode::CREATED,
            Json(json!({ "name": final_name, "slug": slug })),
        )
            .into_response();
    }
    // Quota check before any bytes land: a full gallery costs a JSON reply.
    let limits = state.limits;
    let (used_bytes, used_photos) = {
        let db = state.db.lock().unwrap();
        let m = db.gallery_meta(&slug).unwrap_or(GalleryMeta {
            public: false,
            bytes: 0,
            photos: 0,
        });
        (m.bytes.max(0) as u64, m.photos)
    };
    // Live `.part-*` temps belong to this gallery too: a stalled chunked
    // upload must not let a second one bypass the byte quota.
    let pending = sweep_and_measure_parts(&state, Some(&slug), Some(&temp_path)).await;
    let used_bytes = used_bytes.saturating_add(pending);
    if limits.gallery_bytes > 0 && used_bytes >= limits.gallery_bytes {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full");
    }
    if limits.gallery_photos > 0 && used_photos >= limits.gallery_photos {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "photo limit reached");
    }
    // Chunked uploads know the file size up front: refuse one that cannot
    // fit before accepting any of its bytes.
    if let Some((c, _)) = &chunk {
        if c.total > limits.max_file as u64 {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
        }
        if limits.gallery_bytes > 0 && used_bytes.saturating_add(c.total) > limits.gallery_bytes {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full");
        }
    }
    // Bounded concurrency: refuse outright instead of queueing disk writers.
    // The global cap is shared; the per-IP cap stops one slow client from
    // pinning every global slot (global first, then per-IP: fixed order).
    let _permit = match state.uploads.try_acquire() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::SERVICE_UNAVAILABLE, "busy"),
    };
    let _ip_permit = match crate::state::UploadSlotGuard::try_acquire(
        &state.upload_ips,
        &ip,
        limits.upload_slots_per_ip,
    ) {
        Some(g) => g,
        None => return err(StatusCode::SERVICE_UNAVAILABLE, "busy"),
    };
    if let Some(free) = free_bytes(&state.dir)
        && limits.min_free > 0
        && free < limits.min_free
    {
        return err(StatusCode::INSUFFICIENT_STORAGE, "storage full");
    }
    // One writer per upload id: a retry must not append to the same temp
    // concurrently. A busy retry reports the committed length (the client
    // jumps to it) and leaves the active request alone.
    let _id_guard = match &chunk {
        Some((_, id)) => match crate::state::UploadIdGuard::try_acquire(&state.upload_ids, id) {
            Some(g) => Some(g),
            None => {
                let received = tokio::fs::metadata(&temp_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "upload in progress", "received": received })),
                )
                    .into_response();
            }
        },
        None => None,
    };
    let t_body = Instant::now();
    // Stream into a dot-prefixed temp file: dot names are never listed or
    // served, so a stalled or aborted upload stays invisible. The real name
    // (and its DB row) only appears after a successful `rename`.
    let base_len = chunk.as_ref().map_or(0, |(c, _)| c.start);
    let chunk_len = chunk.as_ref().map(|(c, _)| c.end - c.start + 1);
    let mut out = match &chunk {
        None => match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
        {
            Ok(f) => f,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store"),
        },
        Some((c, _)) => match tokio::fs::metadata(&temp_path).await {
            Ok(md) => {
                let len = md.len();
                if len > c.total {
                    return err(StatusCode::BAD_REQUEST, "upload too large");
                }
                if len != c.start {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "offset mismatch", "received": len })),
                    )
                        .into_response();
                }
                match tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&temp_path)
                    .await
                {
                    Ok(f) => f,
                    Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store"),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if c.start > 0 {
                    // Restart, startup sweep, or expiry: the client restarts
                    // the file with the same upload id.
                    return (StatusCode::GONE, Json(json!({ "error": "upload expired" })))
                        .into_response();
                }
                match tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp_path)
                    .await
                {
                    Ok(f) => f,
                    // Lost a create race: report the committed length.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let len = tokio::fs::metadata(&temp_path)
                            .await
                            .map(|m| m.len())
                            .unwrap_or(0);
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({ "error": "offset mismatch", "received": len })),
                        )
                            .into_response();
                    }
                    Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store"),
                }
            }
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "cannot store"),
        },
    };
    let started = Instant::now();
    let mut next_free_check = FREE_CHECK_EVERY;
    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    loop {
        // Idle timeout: a dead socket must not pin an upload slot forever.
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                drop(out);
                if chunk.is_some() {
                    // Keep the committed prefix: the client resumes it. An
                    // empty first chunk would otherwise pin an idle temp file.
                    if base_len == 0 && written == 0 {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                    }
                    return err(StatusCode::REQUEST_TIMEOUT, "upload interrupted");
                }
                let _ = tokio::fs::remove_file(&temp_path).await;
                return err(StatusCode::REQUEST_TIMEOUT, "upload timed out");
            }
        };
        let Some(data) = next else { break };
        let data = match data {
            Ok(c) => c,
            Err(_) => {
                drop(out);
                if chunk.is_some() {
                    if base_len == 0 && written == 0 {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                    }
                    return err(StatusCode::REQUEST_TIMEOUT, "upload interrupted");
                }
                let _ = tokio::fs::remove_file(&temp_path).await;
                return ApiError::TooLarge.into_response();
            }
        };
        if data.is_empty() {
            continue;
        }
        written += data.len() as u64;
        if chunk_len.is_some_and(|l| written > l) {
            // Bytes outside this request's range: drop this request's writes
            // but keep the chunks committed by earlier requests.
            let _ = out.set_len(base_len).await;
            drop(out);
            return err(StatusCode::BAD_REQUEST, "content-range overrun");
        }
        let accumulated = base_len + written;
        let ok = started.elapsed() <= UPLOAD_DEADLINE
            && chunk.as_ref().is_none_or(|(c, _)| accumulated <= c.total)
            && accumulated <= limits.max_file as u64
            && (limits.gallery_bytes == 0 || used_bytes + accumulated <= limits.gallery_bytes);
        if !ok {
            if base_len == 0 {
                drop(out);
                let _ = tokio::fs::remove_file(&temp_path).await;
            } else {
                // Keep the committed prefix resumable, but do not let this
                // rejected chunk's bytes eat the quota until the TTL sweep.
                let _ = out.set_len(base_len).await;
            }
            return ApiError::TooLarge.into_response();
        }
        if accumulated >= next_free_check {
            next_free_check = accumulated + FREE_CHECK_EVERY;
            if let Some(free) = free_bytes(&state.dir)
                && limits.min_free > 0
                && free < limits.min_free
            {
                if base_len == 0 {
                    drop(out);
                    let _ = tokio::fs::remove_file(&temp_path).await;
                } else {
                    let _ = out.set_len(base_len).await;
                }
                return err(StatusCode::INSUFFICIENT_STORAGE, "storage full");
            }
        }
        if out.write_all(&data).await.is_err() {
            drop(out);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return ApiError::Internal.into_response();
        }
    }
    let accumulated = base_len + written;
    // A chunk shorter than its declared range is a broken request: keep the
    // committed prefix and let the client resend from the real offset.
    if chunk_len.is_some_and(|l| written < l) {
        drop(out);
        return err(StatusCode::REQUEST_TIMEOUT, "upload interrupted");
    }
    // Non-final chunk: report the committed offset; no fsync, no rename.
    if let Some((c, _)) = &chunk
        && accumulated < c.total
    {
        drop(out);
        return (
            StatusCode::ACCEPTED,
            Json(json!({ "received": accumulated })),
        )
            .into_response();
    }
    // Durability: the rename below is atomic, but only a synced temp file
    // survives a crash after the fact.
    if out.sync_all().await.is_err() {
        drop(out);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return ApiError::Internal.into_response();
    }
    drop(out);
    tracing::debug!(
        stage = "body_stream",
        bytes = accumulated,
        latency_ms = format!("{:.2}", t_body.elapsed().as_secs_f64() * 1000.0)
    );
    if accumulated == 0 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return err(StatusCode::BAD_REQUEST, "empty body");
    }
    // Capture time decides listing order; fall back to upload time.
    let created = tokio::task::spawn_blocking({
        let path = temp_path.clone();
        move || crate::exif_time::taken_millis(&path)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(crate::db::now_millis);
    if tokio::fs::rename(&temp_path, state.dir.join(&final_name))
        .await
        .is_err()
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return ApiError::Internal.into_response();
    }
    // The rename is atomic; flush the directory entry so a power cut cannot
    // leave the bytes under the startup-swept `.part-` name.
    if sync_dir(&state.dir).await.is_err() {
        let _ = tokio::fs::remove_file(state.dir.join(&final_name)).await;
        return ApiError::Internal.into_response();
    }
    // Counters are the quota truth: reconcile after the file exists so a
    // concurrent overshoot is corrected by deleting this file again.
    let usage = {
        let mut db = state.db.lock().unwrap();
        db.add_upload(&slug, &final_name, accumulated, created)
    };
    if let Some((bytes, photos)) = usage {
        let over = (limits.gallery_bytes > 0 && bytes as u64 > limits.gallery_bytes)
            || (limits.gallery_photos > 0 && photos > limits.gallery_photos);
        if over {
            let _ = tokio::fs::remove_file(state.dir.join(&final_name)).await;
            let mut db = state.db.lock().unwrap();
            let _ = db.remove_upload(&slug, &final_name, accumulated);
            return err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full");
        }
    } else {
        // Row vanished mid-upload (delete raced us): drop the orphan file.
        let _ = tokio::fs::remove_file(state.dir.join(&final_name)).await;
        return err(StatusCode::NOT_FOUND, "unknown gallery");
    }
    // Bytes are stored and quota-checked: this gallery now exists for real.
    // Disarm the rollback before thumbnail work, so a client disconnect during
    // decoding cannot delete the row (and orphan the file) behind our back.
    if let Some(m) = &mut mint {
        m.keep = true;
    }
    // Best effort, never fails the upload: the client falls back to the
    // original when the preview is missing.
    if thumb_supported(&ext) {
        write_thumb(&state, &state.dir.join(&final_name), &final_name).await;
    }
    tracing::debug!(
        stage = "put_total",
        latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0)
    );
    state
        .uploads_total
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state
        .upload_bytes_total
        .fetch_add(accumulated, std::sync::atomic::Ordering::Relaxed);
    (
        StatusCode::CREATED,
        Json(json!({ "name": final_name, "slug": slug })),
    )
        .into_response()
}
// --- photo serve / delete ---

pub async fn get_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    serve_photo(state, addr, headers, name, raw, false).await
}

/// JPEG preview from `<photos>/.thumbs`. The URL is `<stored_name>.jpg`, so
/// the normal `/photos/` media cookie authorises it. Missing = 404 (the
/// client then falls back to the original).
pub async fn get_thumb(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    serve_photo(state, addr, headers, name, raw, true).await
}

/// Shared photo/thumbnail serving: auth, visibility, lock, rate limit,
/// caching, ETag and ranges are identical for both.
async fn serve_photo(
    state: Arc<AppState>,
    addr: std::net::SocketAddr,
    headers: HeaderMap,
    name: String,
    raw: Option<String>,
    thumb: bool,
) -> Response {
    let t_total = Instant::now();
    // Orphaned files stay on disk but are unreachable: reject names
    // outside a known gallery before touching the filesystem.
    let slug = if thumb {
        let Some(stored) = name.strip_suffix(".jpg") else {
            return err(StatusCode::NOT_FOUND, "not found");
        };
        match photo_slug(stored) {
            Some(s) => s.to_string(),
            None => return err(StatusCode::NOT_FOUND, "not found"),
        }
    } else {
        match photo_slug(&name) {
            Some(s) => s.to_string(),
            None => return err(StatusCode::NOT_FOUND, "not found"),
        }
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
    // `?pass=` and `X-Gallery-Pass` stay accepted (old shared links); a
    // successful unlock sets a viewer cookie so later media needs neither.
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
    let owner = key_ok || media_cookie_ok(&headers, &state, &slug, MediaToken::Owner);
    // A supplied-but-wrong key on a private gallery is a guess: throttle it
    // before the 404, like the private listing, so private photos cannot be
    // probed at read-limit speed. No key (public probing) burns nothing.
    if !meta.public && !owner && key.is_some() && pass_fail_record(&state, &ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
    }
    // Private gallery: photos are invisible without the key or its cookie.
    if !meta.public && !owner {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    // Viewer lock applies per-file too (direct /photos/ links). A live viewer
    // cookie for THIS gallery already proves the password: no Argon2, no URL
    // secret. A wrong key with no pass was not counted yet, so count it once
    // on denial (check_locked already counted a non-empty wrong pass).
    let viewer_cookie = !owner && media_cookie_ok(&headers, &state, &slug, MediaToken::Viewer);
    let mut unlocked = false;
    if !owner && !viewer_cookie {
        match check_locked(&state, ip, &slug, &pass).await {
            Locked::Open => {}
            Locked::Unlocked => unlocked = true,
            Locked::Denied(deny) => {
                if key.is_some() && pass.is_empty() && pass_fail_record(&state, &ip) {
                    return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
                }
                return deny;
            }
        }
    }
    // Anything a non-owner can reach here is either private (never cache) or
    // a public gallery's unlocked bytes. Owner requests, a locked gallery
    // (pass_hash set), and any ?pass=/viewer-cookie request stay private.
    let (locked, version) = {
        let db = state.db.lock().unwrap();
        (
            db.gallery_pass_hash(&slug).is_some(),
            db.gallery_token_version(&slug).unwrap_or(0),
        )
    };
    let cache_control = if meta.public && !owner && !locked && pass.is_empty() && !viewer_cookie {
        "public, max-age=86400"
    } else {
        "private, no-store"
    };
    // A thumb with no listing row is stale (deleted original, restored backup,
    // delete race): never serve it, even though the derived file still exists.
    if thumb {
        let stored = name.strip_suffix(".jpg").unwrap_or_default();
        let exists = {
            let db = state.db.lock().unwrap();
            db.photo_exists(&slug, stored)
        };
        if !exists {
            return err(StatusCode::NOT_FOUND, "not found");
        }
    }
    let t_canon = Instant::now();
    let path = if thumb {
        state.dir.join(".thumbs").join(&name)
    } else {
        state.dir.join(&name)
    };
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
    let md = file.metadata().await.ok();
    let len = md.as_ref().map(|m| m.len()).unwrap_or(0);
    // Length + mtime: enough for a file that never changes in place (a
    // re-upload gets a new name), and costs no read of the bytes.
    let etag = {
        let mtime = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("\"{len:x}-{mtime:x}\"")
    };
    tracing::debug!(
        stage = "serve_open",
        latency_ms = format!("{:.2}", t_canon.elapsed().as_secs_f64() * 1000.0)
    );
    let (ext, ctype) = if thumb {
        // Thumbs are always JPEG, whatever the original was.
        (".jpg".to_string(), "image/jpeg".to_string())
    } else {
        let ext = match name.rfind('.') {
            Some(i) if i > 0 => name[i..].to_lowercase(),
            _ => String::new(),
        };
        // .ogg is ambiguous (Theora video vs Vorbis/Opus audio): sniff content.
        let ctype = if ext == ".ogg" || ext == ".ogv" || ext == ".ogm" || ext == ".opus" {
            sniff_ogg(&canon_file)
                .await
                .unwrap_or_else(|| content_type_for(ext.as_str()))
        } else {
            content_type_for(ext.as_str())
        };
        (ext, ctype.to_string())
    };
    tracing::debug!(
        stage = "serve_total",
        latency_ms = format!("{:.2}", t_total.elapsed().as_secs_f64() * 1000.0)
    );
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
    // A password proven on this request earns the cookie for every later
    // media tag of this gallery.
    if unlocked {
        builder = builder.header(
            header::SET_COOKIE,
            issue_media_token(&state, &slug, MediaToken::Viewer, version),
        );
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
        None => {
            let builder = builder.header(header::ETAG, etag.as_str());
            // Full-file GET only: range requests keep their own semantics.
            if if_none_match(&headers, &etag) {
                return builder
                    .status(StatusCode::NOT_MODIFIED)
                    .body(Body::empty())
                    .unwrap();
            }
            builder
                .header(header::CONTENT_LENGTH, len.to_string())
                .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
                .unwrap()
        }
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
    let size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    match tokio::fs::remove_file(&path).await {
        // Keep the gallery row: a concurrent upload landing between a file
        // count and a row delete orphaned files under a dead slug.
        Ok(()) => {
            // Best effort: an old photo may have no thumb, and a stale one is
            // removed by admin_sweep if this races.
            let thumbs = state.dir.join(".thumbs");
            let _ = tokio::fs::remove_file(thumbs.join(format!("{name}.jpg"))).await;
            // Flush the removal before forgetting the row: a power cut must
            // not bring a deleted photo back.
            if sync_dir(&state.dir).await.is_err() {
                return ApiError::Internal.into_response();
            }
            match sync_dir(&thumbs).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, "thumbnail dir sync failed"),
            }
            let mut db = state.db.lock().unwrap();
            let _ = db.remove_upload(&slug, &name, size);
            drop(db);
            (
                StatusCode::OK,
                Json(json!({ "deleted": name, "gallery_gone": false })),
            )
                .into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            err(StatusCode::NOT_FOUND, "not found")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "cannot delete"),
    }
}

pub async fn fallback_404() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusts(raw: &str, ip: &str) -> bool {
        let peer: IpAddr = ip.parse().unwrap();
        parse_trust_proxy(raw).iter().any(|net| net.contains(&peer))
    }

    #[test]
    fn trust_proxy_accepts_ips_and_cidrs() {
        assert!(trusts("1", "127.0.0.1"));
        assert!(trusts("true", "::1"));
        assert!(!trusts("1", "192.0.2.1"));
        assert!(trusts("10.0.0.0/8, 203.0.113.7", "10.1.2.3"));
        assert!(trusts("10.0.0.0/8, 203.0.113.7", "203.0.113.7"));
        assert!(!trusts("10.0.0.0/8", "192.0.2.1"));
        assert!(!trusts("", "127.0.0.1"));
    }

    #[test]
    fn trust_proxy_ignores_invalid_entries() {
        assert!(!trusts("not-an-ip", "127.0.0.1"));
        assert!(!trusts("10.0.0.0/99", "10.0.0.1"));
    }

    #[test]
    fn client_ip_uses_forwarded_for_only_from_trusted_peer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "198.51.100.1, 203.0.113.9".parse().unwrap(),
        );
        let trusted = parse_trust_proxy("10.0.0.0/8");
        let peer = |ip: &str| Some(ConnectInfo(format!("{ip}:1234").parse().unwrap()));
        let client: IpAddr = "203.0.113.9".parse().unwrap();

        assert_eq!(client_ip_from(&headers, peer("10.1.2.3"), &trusted), client);
        let direct: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(
            client_ip_from(&headers, peer("192.0.2.1"), &trusted),
            direct
        );
    }

    #[test]
    fn client_ip_walks_forwarded_for_through_trusted_hops() {
        let trusted = parse_trust_proxy("127.0.0.1/32, 10.0.0.0/8");
        let peer = |ip: &str| Some(ConnectInfo(format!("{ip}:1234").parse().unwrap()));
        let client: IpAddr = "198.51.100.7".parse().unwrap();
        let mut headers = HeaderMap::new();

        // Edge proxy -> local Caddy -> app; both proxy hops are trusted.
        headers.insert("x-forwarded-for", "198.51.100.7, 10.9.9.9".parse().unwrap());
        assert_eq!(
            client_ip_from(&headers, peer("127.0.0.1"), &trusted),
            client
        );

        // Every hop trusted: the leftmost address is the best client guess.
        headers.insert("x-forwarded-for", "10.5.5.5, 10.9.9.9".parse().unwrap());
        assert_eq!(
            client_ip_from(&headers, peer("127.0.0.1"), &trusted),
            "10.5.5.5".parse::<IpAddr>().unwrap()
        );

        // A malformed hop is not guessed; rate-limit the connection peer.
        headers.insert(
            "x-forwarded-for",
            "198.51.100.7, not-an-ip".parse().unwrap(),
        );
        assert_eq!(
            client_ip_from(&headers, peer("127.0.0.1"), &trusted),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn metrics_allow_only_local_or_direct_trusted_peers() {
        let trusted = parse_trust_proxy("172.16.0.0/12");
        let peer: IpAddr = "172.17.0.1".parse().unwrap();
        let direct = HeaderMap::new();
        assert!(metrics_direct_peer(peer, &direct, &trusted));

        let mut forwarded = HeaderMap::new();
        forwarded.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert!(!metrics_direct_peer(peer, &forwarded, &trusted));

        let public: IpAddr = "203.0.113.9".parse().unwrap();
        assert!(!metrics_direct_peer(public, &direct, &trusted));
    }
}
