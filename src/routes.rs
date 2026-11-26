use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
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

use crate::db::{GalleryAccess, GalleryDb, StoreUpload};
use crate::state::{
    AppState, MediaToken, OWNER_TOKEN_TTL, PASS_CACHE_TTL, UploadBudget, UploadSlotGuard,
    ViewerAdmission, ViewerAttempt, chunk_limited, is_admin, pass_fail_check, pass_fail_record,
    rate_limited, read_limited,
};
use crate::validate::{
    is_slug, normalize_key, percent_decode_bytes, photo_slug, sanitize_filename,
};

/// A stalled upload socket is dropped after this long without a chunk.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceiling on one upload, however fast the client.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(6 * 3600);
/// While more body is expected, each window must at least deliver this many
/// bytes or the upload is aborted as too slow.
const MIN_UPLOAD_BYTES_PER_WINDOW: u64 = 8 * 1024;
const UPLOAD_FLOOR_WINDOW: Duration = Duration::from_secs(60);
/// Abandoned resumable-upload temps count against the gallery quota until
/// this age, then a sweep reclaims them.
const PART_TTL: Duration = Duration::from_secs(3600);
/// A gallery that never received a photo is reaped after this grace period.
const EMPTY_GALLERY_TTL: Duration = Duration::from_secs(3600);
/// Background reaping of expired upload temps. Health checks also run a
/// sweep, but the cleanup must not depend on an external probe.
pub const PART_SWEEP_PERIOD: Duration = Duration::from_secs(600);
/// healthz probes can be frequent; a full `.part-` scan is O(files).
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Available bytes and allocation unit on the data volume.
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `stat` is plain data; the pointer is a valid C string.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = (st.f_frsize as u64).max(1);
        Some(((st.f_bavail as u64).saturating_mul(unit), unit))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    disk_space(path).map(|(free, _)| free)
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
fn media_cookie_ok(
    headers: &HeaderMap,
    state: &AppState,
    slug: &str,
    kind: MediaToken,
    version: i64,
) -> bool {
    let Some(tok) = crate::state::cookie_value(headers, &media_cookie(slug, kind)) else {
        return false;
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

/// JSON endpoints refuse a body that does not declare `application/json`.
/// Bodyless requests stay allowed (resolve with a header key and no body).
// Response is the cold error path.
#[allow(clippy::result_large_err)]
fn require_json(headers: &HeaderMap) -> Result<(), Response> {
    let has_body = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0)
        || headers.contains_key(header::TRANSFER_ENCODING);
    if !has_body {
        return Ok(());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if json {
        Ok(())
    } else {
        Err(err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json",
        ))
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
        if suffix == 0 {
            return Some(Err(()));
        }
        // RFC 9110: a suffix longer than the file means the whole file.
        return Some(Ok((len.saturating_sub(suffix), len - 1)));
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
        ".ogg" | ".oga" => "audio/ogg",
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

/// Test-only pause before thumbnail work, so a unit test can prove upload
/// slots are already free when `write_thumb` runs.
#[cfg(test)]
struct ThumbGate {
    armed: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
static THUMB_GATE: std::sync::LazyLock<ThumbGate> = std::sync::LazyLock::new(|| ThumbGate {
    armed: std::sync::atomic::AtomicBool::new(false),
    entered: tokio::sync::Semaphore::new(0),
    release: tokio::sync::Semaphore::new(0),
});

/// Write `<photos>/.thumbs/<name>.jpg`, best effort. Old photos may have no
/// thumb: the client falls back to the original. Add a backfill CLI only if
/// existing libraries need previews.
async fn write_thumb(state: &AppState, name: &str) {
    let Ok(_permit) = state.thumbs.try_acquire() else {
        return;
    };
    #[cfg(test)]
    if THUMB_GATE.armed.load(std::sync::atomic::Ordering::SeqCst) {
        THUMB_GATE.entered.add_permits(1);
        let _ = THUMB_GATE.release.acquire().await;
    }
    let dir = state.dir.join(".thumbs");
    let dest = dir.join(format!("{name}.jpg"));
    // Write-then-rename: a concurrent GET must never see a half-written JPEG
    let temp = dir.join(format!(".tmp-{}.jpg", uuid::Uuid::new_v4().as_simple()));
    let src = state.dir.join(name);
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

fn upload_throttle(wait: Duration) -> Response {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() != 0);
    (
        [(header::RETRY_AFTER, seconds.to_string())],
        ApiError::TooMany.into_response(),
    )
        .into_response()
}

/// Liveness/readiness probe: DB reachable, photos dir readable, and free
/// space above `GALLY_MIN_FREE_BYTES` (0 = ignore).
pub async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let db_ok = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM galleries) AND EXISTS(SELECT 1 FROM photos)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok()
    };
    let dir_ok = tokio::fs::read_dir(&state.dir).await.is_ok();
    // At most one full sweep per minute; startup and admin_sweep cover the rest.
    let sweep_due = {
        let mut last = state.last_sweep.lock().unwrap();
        if last.elapsed() >= SWEEP_INTERVAL {
            *last = Instant::now();
            true
        } else {
            false
        }
    };
    if sweep_due {
        let _ = sweep_parts(&state, &mut *state.upload_budget.lock().await).await;
    }
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

/// Prometheus scrape from a local socket, never through a proxy.
pub async fn metrics(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !metrics_direct_peer(addr.ip(), &headers, &TRUSTED_PROXIES) {
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

/// A trusted proxy never counts as a local scraper, even without forwarding.
fn metrics_direct_peer(peer: IpAddr, headers: &HeaderMap, trusted: &[ipnet::IpNet]) -> bool {
    peer.is_loopback()
        && !headers.contains_key("x-forwarded-for")
        && !headers.contains_key("forwarded")
        && !headers.contains_key("x-real-ip")
        && !trusted.iter().any(|net| net.contains(&peer))
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
    let mut first = None;
    let mut rightmost_untrusted = None;
    for value in headers.get_all("x-forwarded-for") {
        let Ok(xff) = value.to_str() else {
            rightmost_untrusted = Some(peer);
            continue;
        };
        for hop in xff.split(',').map(str::trim) {
            let Ok(ip) = hop.parse::<IpAddr>() else {
                rightmost_untrusted = Some(peer);
                continue;
            };
            first.get_or_insert(ip);
            if !trusted.iter().any(|net| net.contains(&ip)) {
                rightmost_untrusted = Some(ip);
            }
        }
    }
    rightmost_untrusted.or(first).unwrap_or(peer)
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
    let Some(value) = headers.get("x-gallery-slug") else {
        return Ok(None);
    };
    let Ok(slug) = value.to_str() else {
        return Err(err(StatusCode::BAD_REQUEST, "bad slug"));
    };
    if is_slug(slug) {
        Ok(Some(slug.to_string()))
    } else {
        Err(err(StatusCode::BAD_REQUEST, "bad slug"))
    }
}

/// Viewer password from the header only. Empty when absent (or not UTF-8).
fn request_pass(headers: &HeaderMap) -> String {
    headers
        .get("x-gallery-pass")
        // Header values are ASCII-only on the way out of fetch(), so app.js
        // percent-encodes: decode here (raw old-style values decode to
        // themselves when they hold no `%XX`).
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .map(|p| String::from_utf8(percent_decode_bytes(p)).unwrap_or_default())
        .filter(|p| !p.is_empty())
        .unwrap_or_default()
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

/// Opaque cursor: `<created>:<key>` (split at the first colon). The unit of
/// `created` depends on the caller (photo ms vs admin gallery seconds).
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

fn locked_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "gallery locked", "locked": true })),
    )
        .into_response()
}

/// Missing and private resources must charge a supplied key the same way.
fn missing_read(state: &AppState, headers: &HeaderMap, ip: &IpAddr, msg: &'static str) -> Response {
    if headers.contains_key("x-gallery-key") && pass_fail_record(state, ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
    }
    err(StatusCode::NOT_FOUND, msg)
}

/// `true` means this request proved the password and can issue a cookie
/// signed for this snapshot, but only after checking it is still current.
#[allow(clippy::result_large_err)]
async fn check_viewer(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ip: IpAddr,
    slug: &str,
    pass: String,
    access: &GalleryAccess,
) -> Result<bool, Response> {
    let Some(hash) = access.hash.as_deref() else {
        return Ok(false);
    };
    if media_cookie_ok(headers, state, slug, MediaToken::Viewer, access.version) {
        return Ok(false);
    }
    if pass.is_empty() {
        if request_key(headers).is_some() && pass_fail_record(state, &ip) {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"));
        }
        return Err(locked_response());
    }
    // The owner key also works in the viewer-password field.
    match state.db.lock().unwrap().key_owns_slug(&pass, slug) {
        Ok(true) => return Ok(true),
        Ok(false) => {}
        Err(_) => return Err(ApiError::Internal.into_response()),
    }
    let key = pass_cache_key(hash, &pass);
    if let Some(at) = state.pass_ok.lock().unwrap().get(&key)
        && at.elapsed() < PASS_CACHE_TTL
    {
        return Ok(true);
    }
    let attempt = ViewerAttempt::try_begin(state, ip).map_err(|denied| match denied {
        ViewerAdmission::RateLimited => err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"),
        ViewerAdmission::Busy => err(StatusCode::SERVICE_UNAVAILABLE, "busy"),
    })?;
    let hash = hash.to_vec();
    let ok = tokio::task::spawn_blocking(move || {
        let ok = crate::db::verify_password(&pass, &hash);
        attempt.finish(ok.then_some(key));
        ok
    })
    .await
    .map_err(|_| ApiError::Internal.into_response())?;
    if ok {
        return Ok(true);
    }
    Err(locked_response())
}

#[allow(clippy::result_large_err)]
fn current_access(
    db: &rusqlite::Connection,
    slug: &str,
    initial: &GalleryAccess,
    owner: bool,
    missing: &'static str,
) -> Result<GalleryAccess, Response> {
    match db.gallery_access(slug) {
        Ok(Some(access)) if owner || &access == initial => Ok(access),
        Ok(Some(access)) if access.public => Err(locked_response()),
        Ok(_) => Err(err(StatusCode::NOT_FOUND, missing)),
        Err(_) => Err(ApiError::Internal.into_response()),
    }
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
    let key = request_key(&headers);
    let (access, key_ok) = {
        let db = state.db.lock().unwrap();
        let access = db.gallery_access(&slug);
        let key_ok = match key.as_ref() {
            Some(k) => db.key_owns_slug(k, &slug),
            None => Ok(false),
        };
        (access, key_ok)
    };
    tracing::debug!(
        stage = "list_db_check",
        latency_ms = format!("{:.2}", t_db.elapsed().as_secs_f64() * 1000.0)
    );
    let key_ok = match key_ok {
        Ok(ok) => ok,
        Err(_) => return ApiError::Internal.into_response(),
    };
    let access = match access {
        Ok(Some(access)) => access,
        Ok(None) => return missing_read(&state, &headers, &ip, "unknown gallery"),
        Err(_) => return ApiError::Internal.into_response(),
    };
    let owner = key_ok || media_cookie_ok(&headers, &state, &slug, MediaToken::Owner, 0);
    // Private gallery: nothing exists for anyone but the owner.
    if !access.public && !owner {
        return missing_read(&state, &headers, &ip, "unknown gallery");
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
    let unlocked = if owner {
        false
    } else {
        let pass = request_pass(&headers);
        match check_viewer(&state, &headers, ip, &slug, pass, &access).await {
            Ok(unlocked) => unlocked,
            Err(deny) => return deny,
        }
    };
    let t_scan = Instant::now();
    let (page, current) = {
        let db = state.db.lock().unwrap();
        let current = match current_access(&db, &slug, &access, owner, "unknown gallery") {
            Ok(current) => current,
            Err(deny) => return deny,
        };
        (db.photos_page(&slug, before, limit), current)
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
        "public": current.public,
        "locked": current.hash.is_some(),
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
            access.version,
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
    if let Some(wait) = rate_limited(&state, &ip) {
        return upload_throttle(wait);
    }
    let key = match request_key(&headers) {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    let owned = {
        let db = state.db.lock().unwrap();
        db.key_owns_slug(&key, &slug)
    };
    match owned {
        Ok(true) => {}
        Ok(false) => {
            if pass_fail_record(&state, &ip) {
                return ApiError::TooMany.into_response();
            }
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
        Err(_) => return ApiError::Internal.into_response(),
    }
    if let Err(resp) = require_json(&headers) {
        return resp;
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
    match ok {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "unknown gallery"),
        Err(_) => return ApiError::Internal.into_response(),
    }
    (
        StatusCode::OK,
        Json(json!({ "slug": slug, "public": public })),
    )
        .into_response()
}

// --- admin ---

pub async fn admin_galleries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let query = raw_query.unwrap_or_default();
    let limit = query_param(&query, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let before = match query_param(&query, "before") {
        None => None,
        Some(cursor) => match parse_cursor(&cursor) {
            Some(before) => Some(before),
            None => return err(StatusCode::BAD_REQUEST, "bad cursor"),
        },
    };
    // Counters come from the DB: a directory scan costs O(files).
    let rows: Vec<(String, i64, i64, i64, i64)> = {
        let db = state.db.lock().unwrap();
        let mut stmt = match db.prepare(
            "SELECT slug, created, photos, bytes, public FROM galleries
             WHERE ?1 IS NULL OR created < ?1 OR (created = ?1 AND slug < ?2)
             ORDER BY created DESC, slug DESC LIMIT ?3",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return ApiError::Internal.into_response(),
        };
        let (before_created, before_slug) = match &before {
            Some((created, slug)) => (Some(*created), Some(slug.as_str())),
            None => (None, None),
        };
        let rows = match stmt.query_map(
            rusqlite::params![before_created, before_slug, limit as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ) {
            Ok(rows) => rows,
            Err(_) => return ApiError::Internal.into_response(),
        };
        match rows.collect::<rusqlite::Result<Vec<_>>>() {
            Ok(rows) => rows,
            Err(_) => return ApiError::Internal.into_response(),
        }
    };
    let next = if rows.len() == limit {
        rows.last()
            .map(|(slug, created, ..)| format!("{created}:{slug}"))
    } else {
        None
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
    (
        StatusCode::OK,
        Json(json!({ "galleries": galleries, "next": next })),
    )
        .into_response()
}

/// Drop the admin token; the browser cookie is cleared in the response.
pub async fn admin_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "admin session inactive");
    }
    if let Some(tok) = crate::state::admin_token(&headers) {
        state.admin_tokens.lock().unwrap().remove(&tok);
    }
    let secure = if state.limits.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!("admin=; HttpOnly; SameSite=Lax; Path=/api/admin; Max-Age=0{secure}");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
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
    if let Some(wait) = rate_limited(&state, &ip) {
        return upload_throttle(wait);
    }
    let admin_pass = std::env::var("ADMIN_PASS").unwrap_or_default();
    if admin_pass.is_empty() {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    if let Err(resp) = require_json(&headers) {
        return resp;
    }
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let guess: String = serde_json::from_slice::<UnlockBody>(&bytes)
        .map(|b| b.password)
        .unwrap_or_default();
    use subtle::ConstantTimeEq;
    // Check after the bounded read: pending bodies cannot pre-clear the limit.
    match pass_fail_check(&state, ip, || {
        guess.as_bytes().ct_eq(admin_pass.as_bytes()).unwrap_u8() == 1
    }) {
        Some(true) => {}
        Some(false) => return err(StatusCode::FORBIDDEN, "forbidden"),
        None => return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"),
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
    let cookie =
        format!("admin={tok}; HttpOnly; SameSite=Lax; Path=/api/admin; Max-Age=86400{secure}");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
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
    if let Some(wait) = rate_limited(&state, &ip) {
        return upload_throttle(wait);
    }
    let mut key = request_key(&headers);
    if let Err(resp) = require_json(&headers) {
        return resp;
    }
    // Always read the body: a header key plus a body slug must resolve, not
    // mint. Present-but-invalid slug is a client bug: reject, never mint.
    let bytes = match read_body(body, 8192).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let parsed: Option<serde_json::Value> = if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) if value.is_object() => Some(value),
            _ => return err(StatusCode::BAD_REQUEST, "bad request"),
        }
    };
    let mut slug_param: Option<String> = None;
    if let Some(value) = parsed.as_ref() {
        // A present "slug" must be a valid slug after trimming; only a missing
        // key means "no slug". Present-but-invalid is a client bug: reject,
        // never mint.
        match value.get("slug") {
            None => {}
            Some(serde_json::Value::String(s)) => {
                let s = s.trim();
                if !is_slug(s) {
                    return err(StatusCode::BAD_REQUEST, "bad slug");
                }
                slug_param = Some(s.to_string());
            }
            Some(_) => return err(StatusCode::BAD_REQUEST, "bad request"),
        }
        if let Some(value) = value.get("key") {
            match value.as_str() {
                Some(raw) => {
                    if key.is_none() {
                        key = normalize_key(raw);
                    }
                }
                None => return err(StatusCode::BAD_REQUEST, "bad request"),
            }
        }
    }
    let key = match key {
        Some(k) => k,
        None => return ApiError::Unauthorized.into_response(),
    };
    if let Some(t) = slug_param {
        let ok = {
            let db = state.db.lock().unwrap();
            match db.gallery_exists(&t) {
                Ok(true) => db.key_owns_slug(&key, &t),
                Ok(false) => Ok(false),
                Err(_) => return ApiError::Internal.into_response(),
            }
        };
        return match ok {
            Ok(true) => (StatusCode::OK, Json(json!({ "slug": t }))).into_response(),
            Ok(false) => {
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
            Err(_) => ApiError::Internal.into_response(),
        };
    }
    if !invite_ok(&state, &headers) {
        if pass_fail_record(&state, &ip) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
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
    // Any valid slug gets the SPA: 404 here for an unknown slug would leak
    // whether a private gallery exists. The client redirects home when its
    // listing 404s.
    if !is_slug(&slug) {
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

/// Reap expired parts and account for idle parts under the admission lock.
async fn sweep_parts(state: &AppState, budget: &mut UploadBudget) -> std::io::Result<()> {
    let mut rd = tokio::fs::read_dir(&state.dir).await?;
    let mut idle: std::collections::HashMap<_, _> = budget
        .idle
        .iter()
        .filter(|(id, _)| budget.active.contains_key(*id))
        .map(|(id, len)| (id.clone(), *len))
        .collect();
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name.strip_prefix(".part-") {
            if budget.active.contains_key(id) {
                continue;
            }
            let md = match ent.metadata().await {
                Ok(md) => md,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let expired = md
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= PART_TTL);
            if expired && tokio::fs::remove_file(ent.path()).await.is_ok() {
                budget.damaged.remove(id);
            } else {
                idle.insert(id.to_string(), md.len());
            }
        }
    }
    budget
        .damaged
        .retain(|id| idle.contains_key(id) || budget.active.contains_key(id));
    budget.idle = idle;
    Ok(())
}

/// Reap only confirmed unused galleries, never legacy unknown or emptied URLs.
/// A preserved unindexed original or any live upload keeps the gallery.
async fn sweep_empty_galleries(state: &AppState) -> Result<usize, ()> {
    let cutoff = crate::db::now_millis() / 1000 - EMPTY_GALLERY_TTL.as_secs() as i64;
    let mut candidates: HashSet<String> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare(
                "SELECT slug FROM galleries WHERE ever_stored = 0 AND photos = 0 AND created < ?1",
            )
            .map_err(|_| ())?;
        let rows = match stmt.query_map([cutoff], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(_) => return Err(()),
        };
        match rows.collect::<rusqlite::Result<HashSet<_>>>() {
            Ok(rows) => rows,
            Err(_) => return Err(()),
        }
    };
    if candidates.is_empty() {
        return Ok(0);
    }
    let mut removed = HashSet::new();
    {
        // The admission lock makes the active/upload and file checks atomic
        // with a new upload's target check.
        let budget = state.upload_budget.lock().await;
        for slot in budget.active.keys() {
            if let Some((slug, _)) = slot.split_once('_') {
                candidates.remove(slug);
            }
        }
        let mut entries = tokio::fs::read_dir(&state.dir).await.map_err(|_| ())?;
        while let Some(entry) = entries.next_entry().await.map_err(|_| ())? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let name = name
                .strip_prefix(".part-")
                .or_else(|| name.strip_prefix(crate::db::PENDING_DELETE_PREFIX))
                .unwrap_or(&name);
            if let Some((slug, _)) = name.split_once('_') {
                candidates.remove(slug);
            }
        }
        for slug in candidates {
            let removed_row = state
                .db
                .lock()
                .unwrap()
                .execute(
                    "DELETE FROM galleries WHERE slug = ?1 AND ever_stored = 0 AND photos = 0 AND bytes = 0
                     AND NOT EXISTS (SELECT 1 FROM photos WHERE slug = ?1)",
                    [&slug],
                )
                .map_err(|_| ())?;
            if removed_row != 0 {
                removed.insert(slug);
            }
        }
    }
    if !removed.is_empty() {
        match tokio::fs::read_dir(state.dir.join(".thumbs")).await {
            Ok(mut entries) => loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .split_once('_')
                            .is_some_and(|(slug, _)| removed.contains(slug))
                            && let Err(error) = tokio::fs::remove_file(entry.path()).await
                        {
                            tracing::warn!(%error, path = %entry.path().display(), "remove retired gallery preview");
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(%error, "scan retired gallery previews");
                        break;
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "open retired gallery previews"),
        }
    }
    Ok(removed.len())
}

/// Reap expired upload temps even when no health checker is configured.
/// Runs until the process exits; `every` is injectable for tests.
pub async fn part_sweeper(state: Arc<AppState>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        {
            let mut budget = state.upload_budget.lock().await;
            if let Err(e) = sweep_parts(&state, &mut budget).await {
                tracing::warn!(error = %e, "upload temp sweep failed");
            }
        }
        match sweep_empty_galleries(&state).await {
            Ok(0) => {}
            Ok(galleries) => tracing::info!(galleries, "removed empty galleries"),
            Err(()) => tracing::warn!("empty gallery sweep failed"),
        }
    }
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
    let mut budget = state.upload_budget.lock().await;
    let prefix = format!("{slug}_");
    let part_prefix = format!(".part-{slug}_");
    let pending_prefix = format!("{}{slug}_", crate::db::PENDING_DELETE_PREFIX);
    let thumbs = state.dir.join(".thumbs");
    let (mut removed, mut failed) = sweep_dir(&state.dir, &prefix).await;
    let (part_removed, part_failed) = sweep_dir(&state.dir, &part_prefix).await;
    removed += part_removed;
    failed += part_failed;
    let (pending_removed, pending_failed) = sweep_dir(&state.dir, &pending_prefix).await;
    removed += pending_removed;
    failed += pending_failed;
    let (thumb_removed, thumb_failed) = sweep_dir(&thumbs, &prefix).await;
    removed += thumb_removed;
    failed += thumb_failed;
    // A concurrent upload may land mid-sweep: re-scan, never claim success
    // while files remain. A scan error means we cannot prove the dirs are clean.
    let remaining = has_prefixed(&state.dir, &prefix).await
        || has_prefixed(&state.dir, &part_prefix).await
        || has_prefixed(&state.dir, &pending_prefix).await
        || has_prefixed(&thumbs, &prefix).await;
    if !remaining {
        budget.idle.retain(|id, _| !id.starts_with(&prefix));
        budget.damaged.retain(|id| !id.starts_with(&prefix));
        budget
            .unindexed
            .retain(|name, _| !name.starts_with(&prefix));
    }
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
    let budget = state.upload_budget.lock().await;
    let removed = {
        let mut db = state.db.lock().unwrap();
        db.remove_gallery(&slug)
    };
    drop(budget);
    match removed {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "unknown gallery"),
        Err(_) => return ApiError::Internal.into_response(),
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
    if let Some(denial) = owner_delete_denial(&state, &headers, addr, &slug) {
        return denial;
    }
    // Preflight before touching the row: if the dir cannot be read the files
    // cannot be swept, so keep both the row and its files untouched.
    if tokio::fs::read_dir(&state.dir).await.is_err() {
        return ApiError::Internal.into_response();
    }
    let budget = state.upload_budget.lock().await;
    let removed = {
        let mut db = state.db.lock().unwrap();
        db.remove_gallery(&slug)
    };
    drop(budget);
    match removed {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "not found"),
        Err(_) => return ApiError::Internal.into_response(),
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
            let mut budget = state.upload_budget.lock().await;
            if !is_slug(slug) || budget.active.contains_key(rest) {
                continue;
            }
            let exists = {
                let db = state.db.lock().unwrap();
                db.gallery_exists(slug)
            };
            match exists {
                Ok(true) => continue,
                Ok(false) => {}
                // Never guess "missing" from a failed read.
                Err(_) => return Err(()),
            }
            if tokio::fs::remove_file(ent.path()).await.is_ok() {
                budget.idle.remove(rest);
                budget.damaged.remove(rest);
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
        let mut budget = state.upload_budget.lock().await;
        {
            let db = state.db.lock().unwrap();
            match db.gallery_exists(slug) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => return Err(()),
            }
        }
        if tokio::fs::remove_file(ent.path()).await.is_ok() {
            if dir == state.dir {
                budget.unindexed.remove(&name);
            }
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
        let mut stmt = match db.prepare("SELECT slug FROM galleries") {
            Ok(stmt) => stmt,
            Err(_) => return ApiError::Internal.into_response(),
        };
        let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(_) => return ApiError::Internal.into_response(),
        };
        let mut known = std::collections::HashSet::new();
        for row in rows {
            match row {
                Ok(slug) => {
                    known.insert(slug);
                }
                // An incomplete snapshot could mark a live gallery as an
                // orphan, so a failed read aborts the whole sweep.
                Err(_) => return ApiError::Internal.into_response(),
            }
        }
        known
    };
    let mut deleted: usize = 0;
    let mut failed: usize = 0;
    // Reap expired temps first; the orphan pass below handles the rest.
    let _ = sweep_parts(&state, &mut *state.upload_budget.lock().await).await;
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
    let galleries = match sweep_empty_galleries(&state).await {
        Ok(galleries) => galleries,
        Err(()) => return ApiError::Internal.into_response(),
    };
    (
        StatusCode::OK,
        Json(json!({ "deleted": deleted, "failed": failed, "galleries": galleries })),
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
    if let Some(wait) = rate_limited(&state, &ip) {
        return upload_throttle(wait);
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
    match known {
        Ok(true) => {}
        Ok(false) => {
            if pass_fail_record(&state, &ip) {
                return ApiError::TooMany.into_response();
            }
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
        Err(_) => return ApiError::Internal.into_response(),
    }
    if let Err(resp) = require_json(&headers) {
        return resp;
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
        let Ok(permit) = Arc::clone(&state.argon2).try_acquire_owned() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "busy");
        };
        let h = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::db::hash_password(&password)
        })
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
    match ok {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "unknown gallery"),
        Err(_) => return ApiError::Internal.into_response(),
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
    // One file arrives as many requests: the upload bucket covers only its
    // first chunk (or a legacy full-body PUT), every chunk hits its own bucket.
    let first_or_legacy = match &chunk {
        None => true,
        Some((c, _)) => c.start == 0,
    };
    if chunk.is_some()
        && let Some(wait) = chunk_limited(&state, &ip)
    {
        return upload_throttle(wait);
    }
    if first_or_legacy && let Some(wait) = rate_limited(&state, &ip) {
        return upload_throttle(wait);
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
                let exists = match db.gallery_exists(t) {
                    Ok(exists) => exists,
                    Err(_) => return ApiError::Internal.into_response(),
                };
                if !exists {
                    drop(db);
                    let ip = client_ip(&headers, Some(ConnectInfo(addr)));
                    if pass_fail_record(&state, &ip) {
                        return ApiError::TooMany.into_response();
                    }
                    return err(StatusCode::NOT_FOUND, "unknown gallery");
                }
                match db.key_owns_slug(k, t) {
                    Ok(true) => t.clone(),
                    Ok(false) => {
                        drop(db);
                        let ip = client_ip(&headers, Some(ConnectInfo(addr)));
                        if pass_fail_record(&state, &ip) {
                            return ApiError::TooMany.into_response();
                        }
                        return err(StatusCode::NOT_FOUND, "unknown gallery");
                    }
                    Err(_) => return ApiError::Internal.into_response(),
                }
            }
            // Home: always mint a fresh gallery, never reuse.
            (None, k) => {
                if !invite_ok(&state, &headers) {
                    drop(db);
                    if pass_fail_record(&state, &ip) {
                        return ApiError::TooMany.into_response();
                    }
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
    let (sanitized, ext) = sanitize_filename(&raw_name);
    let (chunk, id) = match chunk {
        Some((chunk, id)) => (Some(chunk), id),
        None => (None, uuid::Uuid::new_v4().as_simple().to_string()),
    };
    let slot = format!("{slug}_{id}");
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse().ok());
    let upload = Upload {
        part: state.dir.join(format!(".part-{slot}")),
        name: format!("{slot}_{sanitized}"),
        slot,
        slug,
        chunk,
        content_length,
    };
    // A chunk past the start needs the temp an earlier chunk created. Check
    // cheaply here; save_upload repeats it for the race with an expiry sweep.
    if upload.chunk.is_some_and(|c| c.start > 0) {
        match tokio::fs::symlink_metadata(&upload.part).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // The part may already be the final file: a lost reply must
                // still reach save_upload's idempotent recovery.
                match tokio::fs::symlink_metadata(&state.dir.join(&upload.name)).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return err(StatusCode::GONE, "upload expired");
                    }
                    Err(e) => return store_error(e),
                }
            }
            Err(e) => return store_error(e),
        }
    }
    let (mut reply, response) = tokio::sync::oneshot::channel();
    // A canceled HTTP future cannot drop a Tokio file with writes in flight
    // or release the writer slot before rollback. Only body polling is cancelable.
    tokio::spawn(async move {
        let mut mint = target.is_none().then(|| MintGuard {
            state: &state,
            slug: upload.slug.clone(),
            keep: false,
        });
        let result = run_upload(&state, &upload, ip, &ext, body, &mut reply, &mut mint).await;
        drop(mint);
        tracing::debug!(
            stage = "put_total",
            latency_ms = t_total.elapsed().as_secs_f64() * 1000.0
        );
        let _ = reply.send(result);
    });
    response
        .await
        .unwrap_or_else(|_| ApiError::Internal.into_response())
}

struct Upload {
    part: PathBuf,
    name: String,
    slot: String,
    slug: String,
    chunk: Option<UploadChunk>,
    content_length: Option<u64>,
}

fn created_upload(upload: &Upload) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({ "name": upload.name, "slug": upload.slug })),
    )
        .into_response()
}

fn storage_full() -> Response {
    err(StatusCode::INSUFFICIENT_STORAGE, "storage full")
}

fn gallery_full() -> Response {
    err(StatusCode::PAYLOAD_TOO_LARGE, "gallery storage full")
}

fn interrupted() -> Response {
    err(StatusCode::REQUEST_TIMEOUT, "upload interrupted")
}

fn store_error(e: std::io::Error) -> Response {
    if e.raw_os_error() == Some(libc::ENOSPC) {
        storage_full()
    } else {
        ApiError::Internal.into_response()
    }
}

#[allow(clippy::result_large_err)]
fn reserve_bytes(
    state: &AppState,
    budget: &mut UploadBudget,
    upload: &Upload,
    bytes: u64,
    unwritten: u64,
) -> Result<(), Response> {
    let limits = state.limits;
    let meta = match state.db.lock().unwrap().gallery_meta(&upload.slug) {
        Ok(Some(meta)) => meta,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "unknown gallery")),
        Err(_) => return Err(ApiError::Internal.into_response()),
    };
    let in_gallery = |id: &str| {
        id.split_once('_')
            .is_some_and(|(slug, _)| slug == upload.slug)
    };
    if limits.gallery_bytes > 0 {
        let used = budget
            .active
            .iter()
            .filter(|(id, _)| in_gallery(id))
            .map(|(_, reserve)| reserve.bytes)
            .chain(
                budget
                    .idle
                    .iter()
                    .filter(|(id, _)| **id != upload.slot && in_gallery(id))
                    .map(|(_, n)| *n),
            )
            .chain(
                budget
                    .unindexed
                    .iter()
                    .filter(|(name, _)| **name != upload.name && in_gallery(name))
                    .map(|(_, n)| *n),
            )
            .fold(
                (meta.bytes.max(0) as u64).saturating_add(bytes),
                u64::saturating_add,
            );
        if used > limits.gallery_bytes {
            return Err(gallery_full());
        }
    }
    if limits.gallery_photos > 0 && meta.photos >= limits.gallery_photos {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "photo limit reached"));
    }
    if cfg!(unix) && limits.min_free > 0 {
        let Some((free, unit)) = disk_space(&state.dir) else {
            return Err(storage_full());
        };
        let round = |bytes: u64| bytes.div_ceil(unit).saturating_mul(unit);
        let needed = budget
            .active
            .iter()
            .fold(limits.min_free, |needed, (id, reserve)| {
                let extra = if id == &upload.slot { unwritten } else { 0 };
                let end = reserve
                    .flushed
                    .saturating_add(reserve.unwritten)
                    .saturating_add(extra);
                needed.saturating_add(round(end) - round(reserve.flushed))
            });
        if free < needed {
            return Err(storage_full());
        }
    }
    let reserve = budget
        .active
        .get_mut(&upload.slot)
        .expect("registered writer");
    reserve.bytes += bytes;
    reserve.unwritten += unwritten;
    Ok(())
}

async fn run_upload(
    state: &AppState,
    upload: &Upload,
    ip: IpAddr,
    ext: &str,
    body: Body,
    reply: &mut tokio::sync::oneshot::Sender<Response>,
    mint: &mut Option<MintGuard<'_>>,
) -> Response {
    let mut budget = tokio::select! {
        _ = reply.closed() => return interrupted(),
        budget = state.upload_budget.lock() => budget,
    };
    if budget.active.contains_key(&upload.slot) {
        return err(StatusCode::CONFLICT, "upload in progress");
    }
    let Ok(permit) = state.uploads.try_acquire() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "busy");
    };
    let Some(ip_permit) =
        UploadSlotGuard::try_acquire(&state.upload_ips, &ip, state.limits.upload_slots_per_ip)
    else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "busy");
    };
    budget
        .active
        .insert(upload.slot.clone(), Default::default());
    let result = save_upload(state, upload, budget, body, reply, mint).await;
    let mut budget = state.upload_budget.lock().await;
    if let Ok(md) = tokio::fs::metadata(&upload.part).await {
        budget.idle.insert(upload.slot.clone(), md.len());
    } else {
        budget.idle.remove(&upload.slot);
    }
    budget.active.remove(&upload.slot);
    drop(budget);
    // Slots cover streaming and indexing only: a thumbnail decode must not
    // hold upload capacity while other clients wait.
    drop(ip_permit);
    drop(permit);

    match result {
        Ok(Some(bytes)) => {
            if thumb_supported(ext) {
                write_thumb(state, &upload.name).await;
            }
            state
                .uploads_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            state
                .upload_bytes_total
                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
            created_upload(upload)
        }
        Ok(None) => created_upload(upload),
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
async fn save_upload(
    state: &AppState,
    upload: &Upload,
    mut budget: tokio::sync::MutexGuard<'_, UploadBudget>,
    body: Body,
    reply: &mut tokio::sync::oneshot::Sender<Response>,
    mint: &mut Option<MintGuard<'_>>,
) -> Result<Option<u64>, Response> {
    // Targeted checks instead of a full sweep on every chunk. The part stat
    // also keeps this slot's idle quota accounting current.
    let part_len = match tokio::fs::symlink_metadata(&upload.part).await {
        Ok(md) => Some(md.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(store_error(e)),
    };
    if let Some(len) = part_len {
        budget.idle.insert(upload.slot.clone(), len);
    } else {
        // No part means no partial offset to protect; otherwise a damaged
        // slot would 500 until the next healthz sweep.
        budget.idle.remove(&upload.slot);
        budget.damaged.remove(&upload.slot);
    }
    if upload.chunk.is_some() {
        let pending = state.dir.join(format!(
            "{}{}",
            crate::db::PENDING_DELETE_PREFIX,
            upload.name
        ));
        match tokio::fs::symlink_metadata(pending).await {
            Ok(_) => return Err(err(StatusCode::CONFLICT, "upload id already used")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(store_error(e)),
        }
    }
    let prefix = format!("{}_", upload.slot);
    let indexed = {
        let db = state.db.lock().unwrap();
        db.upload_name(&upload.slug, &prefix)
            .map_err(|_| ApiError::Internal.into_response())?
    };
    let final_path = state.dir.join(&upload.name);
    // An already-renamed final may be unindexed after a crash: recover its row.
    let final_meta = match tokio::fs::symlink_metadata(&final_path).await {
        Ok(md) => Some(md),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(store_error(e)),
    };
    // shortcut: this check is exact-name only — the client must keep the
    // filename stable for one upload id; a post-crash stray under a different
    // sanitized name is not detected (indexed finals do come from the prefix).
    if indexed.is_some() || final_meta.is_some() {
        let Some(chunk) = upload.chunk else {
            return Err(ApiError::Internal.into_response());
        };
        if indexed.as_ref().is_some_and(|name| name != &upload.name) {
            return Err(err(StatusCode::CONFLICT, "upload id already used"));
        }
        let Some(md) = final_meta else {
            return Err(err(StatusCode::CONFLICT, "upload id already used"));
        };
        if !md.is_file() || md.len() != chunk.total {
            return Err(err(StatusCode::CONFLICT, "upload id already used"));
        }
        if indexed.is_some() {
            let size = state
                .db
                .lock()
                .unwrap()
                .photo_size(&upload.slug, &upload.name);
            return if size.ok().flatten().and_then(|n| u64::try_from(n).ok()) == Some(chunk.total) {
                Ok(None)
            } else {
                Err(ApiError::Internal.into_response())
            };
        }
        // The bytes survived an indexing error. Recover the row without replacing them.
        budget.unindexed.insert(upload.name.clone(), chunk.total);
        reserve_bytes(state, &mut budget, upload, chunk.total, 0)?;
        budget.unindexed.remove(&upload.name);
        drop(budget);
        let created = capture_time(state, &final_path).await;
        let mut budget = state.upload_budget.lock().await;
        // An explicit delete may have won during EXIF extraction.
        match tokio::fs::symlink_metadata(&final_path).await {
            Ok(md) if md.is_file() && md.len() == chunk.total => {}
            Ok(_) => return Err(ApiError::Internal.into_response()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(err(StatusCode::GONE, "upload expired"));
            }
            Err(e) => {
                budget.unindexed.insert(upload.name.clone(), chunk.total);
                return Err(store_error(e));
            }
        }
        if let Err(response) = async {
            sync_dir(&state.dir).await.map_err(store_error)?;
            record_upload(state, &mut budget, upload, chunk.total, created)
        }
        .await
        {
            budget.unindexed.insert(upload.name.clone(), chunk.total);
            budget
                .active
                .get_mut(&upload.slot)
                .expect("registered writer")
                .bytes = 0;
            return Err(response);
        }
        return Ok(Some(chunk.total));
    }

    let base = budget.idle.get(&upload.slot).copied();
    if budget.damaged.contains(&upload.slot) {
        return Err(ApiError::Internal.into_response());
    }
    let base_len = base.unwrap_or(0);
    budget
        .active
        .get_mut(&upload.slot)
        .expect("registered writer")
        .flushed = base_len;
    if let Some(chunk) = upload.chunk {
        if chunk.total > state.limits.max_file as u64 {
            return Err(ApiError::TooLarge.into_response());
        }
        if base.is_none() && chunk.start > 0 {
            return Err(err(StatusCode::GONE, "upload expired"));
        }
        if base_len > chunk.total {
            return Err(err(StatusCode::BAD_REQUEST, "upload too large"));
        }
        if base_len != chunk.start {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": "offset mismatch", "received": base_len })),
            )
                .into_response());
        }
        if let Some(len) = upload.content_length {
            let expected = chunk.end - chunk.start + 1;
            if len > expected {
                return Err(err(StatusCode::BAD_REQUEST, "content-range overrun"));
            }
            if len < expected {
                return Err(interrupted());
            }
        }
        reserve_bytes(
            state,
            &mut budget,
            upload,
            chunk.total,
            chunk.total - base_len,
        )?;
    } else {
        if base.is_some() {
            return Err(ApiError::Internal.into_response());
        }
        let len = upload.content_length.unwrap_or(0);
        if len > state.limits.max_file as u64 {
            return Err(ApiError::TooLarge.into_response());
        }
        reserve_bytes(state, &mut budget, upload, len, len)?;
    }
    let mut out = tokio::fs::OpenOptions::new()
        .append(true)
        .create_new(base.is_none())
        .open(&upload.part)
        .await
        .map_err(store_error)?;
    budget.idle.remove(&upload.slot);
    drop(budget);

    let bytes = match stream_upload(state, upload, &mut out, body, reply, base_len).await {
        Ok(bytes) => bytes,
        Err(response) => {
            // `write_all` can finish before Tokio's background write. Settle it
            // even on error before touching the old prefix or releasing the ID.
            let _ = out.flush().await;
            drop(out);
            return Err(rollback_part(state, upload, base_len, response).await);
        }
    };
    drop(out);
    if upload.chunk.is_some_and(|chunk| bytes < chunk.total) {
        return Err((StatusCode::ACCEPTED, Json(json!({ "received": bytes }))).into_response());
    }
    let created = capture_time(state, &upload.part).await;
    let mut budget = state.upload_budget.lock().await;
    // Confirm the caps before moving bytes where recount can see them.
    // Indexing checks the caps again in the insertion transaction.
    let meta = state.db.lock().unwrap().gallery_meta(&upload.slug);
    let rejected = match meta {
        Ok(Some(meta))
            if state.limits.gallery_photos > 0 && meta.photos >= state.limits.gallery_photos =>
        {
            Some(err(StatusCode::PAYLOAD_TOO_LARGE, "photo limit reached"))
        }
        Ok(Some(meta))
            if state.limits.gallery_bytes > 0
                && (meta.bytes.max(0) as u64).saturating_add(bytes)
                    > state.limits.gallery_bytes =>
        {
            Some(gallery_full())
        }
        Ok(None) => Some(err(StatusCode::NOT_FOUND, "unknown gallery")),
        _ => None, // A DB failure must still preserve the received original for recount.
    };
    if let Some(deny) = rejected {
        return Err(rollback_part_locked(&mut budget, upload, base_len, deny).await);
    }
    match tokio::fs::symlink_metadata(&final_path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        _ => {
            drop(budget);
            return Err(
                rollback_part(state, upload, base_len, ApiError::Internal.into_response()).await,
            );
        }
    }
    if let Err(e) = tokio::fs::rename(&upload.part, &final_path).await {
        drop(budget);
        return Err(rollback_part(state, upload, base_len, store_error(e)).await);
    }
    // Flush the rename before committing the DB row.
    let result = match sync_dir(&state.dir).await {
        Ok(()) => record_upload(state, &mut budget, upload, bytes, created),
        Err(e) => {
            tracing::warn!(error = %e, "upload directory sync failed");
            Err(ApiError::Internal.into_response())
        }
    };
    if let Err(response) = result {
        if response.status().is_server_error() {
            // Preserve the original and its gallery after a durability/DB failure.
            budget.unindexed.insert(upload.name.clone(), bytes);
            budget
                .active
                .get_mut(&upload.slot)
                .expect("registered writer")
                .bytes = 0;
            if let Some(mint) = mint {
                mint.keep = true;
            }
            return Err(response);
        }
        tokio::fs::rename(&final_path, &upload.part)
            .await
            .map_err(store_error)?;
        let response = rollback_part_locked(&mut budget, upload, base_len, response).await;
        sync_dir(&state.dir).await.map_err(store_error)?;
        return Err(response);
    }
    if let Some(mint) = mint {
        mint.keep = true;
    }
    Ok(Some(bytes))
}

/// Per-request slow-client floor: bytes received by this request per fixed
/// 60-second window. The floor shrinks to the request's declared size, so a
/// small resumable chunk is judged against its own remaining bytes.
/// A body with no declared length that only completes after its first window
/// can be rejected: the final frame is not known to be final until the stream
/// ends.
/// shortcut: 8 KiB/60 s (~137 B/s) rejects genuinely slower links; raise the
/// floor if large uploads over very slow connections matter.
struct UploadFloor {
    window_start: Instant,
    window_bytes: u64,
    floor: u64,
}

impl UploadFloor {
    /// `total` is the bytes this request still expects, when declared.
    fn new(now: Instant, total: Option<u64>) -> Self {
        Self {
            window_start: now,
            window_bytes: 0,
            floor: total.map_or(MIN_UPLOAD_BYTES_PER_WINDOW, |total| {
                total.min(MIN_UPLOAD_BYTES_PER_WINDOW)
            }),
        }
    }

    /// Account for `bytes` that arrived at `now`. Returns false when a window
    /// closed under the floor while the body was incomplete. `complete`
    /// exempts the frame that delivered the declared range, so a small final
    /// chunk is never rejected.
    fn record(&mut self, now: Instant, bytes: u64, complete: bool) -> bool {
        self.window_bytes = self.window_bytes.saturating_add(bytes);
        if now.duration_since(self.window_start) < UPLOAD_FLOOR_WINDOW {
            return true;
        }
        if self.window_bytes >= self.floor {
            self.window_start = now;
            self.window_bytes = 0;
            return true;
        }
        complete
    }
}

#[allow(clippy::result_large_err)]
async fn stream_upload(
    state: &AppState,
    upload: &Upload,
    out: &mut tokio::fs::File,
    body: Body,
    reply: &mut tokio::sync::oneshot::Sender<Response>,
    base_len: u64,
) -> Result<u64, Response> {
    let deadline = tokio::time::Instant::now() + UPLOAD_DEADLINE;
    let end = upload.chunk.map(|c| c.end + 1).or(upload.content_length);
    let mut floor = UploadFloor::new(Instant::now(), end.map(|end| end.saturating_sub(base_len)));
    let mut stream = body.into_data_stream();
    let mut bytes = base_len;
    let mut unsettled = 0;
    while let Some(data) = tokio::select! {
        biased;
        _ = reply.closed() => return Err(interrupted()),
        next = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + IDLE_TIMEOUT), stream.next()) => next.map_err(|_| interrupted())?,
    } {
        let data = data.map_err(|_| interrupted())?;
        if data.is_empty() {
            continue;
        }
        let len = data.len() as u64;
        bytes = bytes.saturating_add(len);
        if upload.chunk.is_some_and(|c| bytes > c.end + 1) {
            return Err(err(StatusCode::BAD_REQUEST, "content-range overrun"));
        }
        if upload.chunk.is_none() && end.is_some_and(|end| bytes > end) {
            return Err(err(StatusCode::BAD_REQUEST, "content-length overrun"));
        }
        if bytes > state.limits.max_file as u64 {
            return Err(ApiError::TooLarge.into_response());
        }
        let complete = end.is_some_and(|end| bytes == end);
        if !floor.record(Instant::now(), len, complete) {
            return Err(interrupted());
        }
        if end.is_none() {
            reserve_bytes(
                state,
                &mut *state.upload_budget.lock().await,
                upload,
                len,
                len,
            )?;
        }
        out.write_all(&data).await.map_err(store_error)?;
        unsettled += len;
        // Keep unflushed disk credit conservative without waiting on every frame.
        if state.limits.min_free > 0 && unsettled >= 64 * 1024 {
            out.flush().await.map_err(store_error)?;
            let mut budget = state.upload_budget.lock().await;
            budget
                .active
                .get_mut(&upload.slot)
                .expect("registered writer")
                .flush_bytes(unsettled);
            unsettled = 0;
        }
        if complete && upload.chunk.is_none() {
            // Only legacy whole-body uploads may finish before EOF.
            break;
        }
    }
    if end.is_some_and(|end| bytes != end) {
        return Err(interrupted());
    }
    if bytes == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "empty body"));
    }
    out.flush().await.map_err(store_error)?;
    if state.limits.min_free > 0 && unsettled > 0 {
        let mut budget = state.upload_budget.lock().await;
        budget
            .active
            .get_mut(&upload.slot)
            .expect("registered writer")
            .flush_bytes(unsettled);
    }
    if upload.chunk.is_none_or(|c| bytes == c.total) {
        out.sync_all().await.map_err(store_error)?;
    }
    Ok(bytes)
}

async fn rollback_part(
    state: &AppState,
    upload: &Upload,
    base_len: u64,
    response: Response,
) -> Response {
    let mut budget = state.upload_budget.lock().await;
    rollback_part_locked(&mut budget, upload, base_len, response).await
}

async fn rollback_part_locked(
    budget: &mut UploadBudget,
    upload: &Upload,
    base_len: u64,
    response: Response,
) -> Response {
    let result = if base_len == 0 {
        tokio::fs::remove_file(&upload.part).await
    } else {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .open(&upload.part)
            .await
        {
            Ok(file) => file.set_len(base_len).await,
            Err(e) => Err(e),
        }
    };
    if let Err(e) = result
        && e.kind() != std::io::ErrorKind::NotFound
    {
        budget.damaged.insert(upload.slot.clone());
        return store_error(e);
    }
    response
}

async fn capture_time(state: &AppState, path: &Path) -> i64 {
    let Ok(_permit) = state.capture_times.acquire().await else {
        return crate::db::now_millis();
    };
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || crate::exif_time::taken_millis(&path))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(crate::db::now_millis)
}

#[allow(clippy::result_large_err)]
fn record_upload(
    state: &AppState,
    budget: &mut UploadBudget,
    upload: &Upload,
    bytes: u64,
    created: i64,
) -> Result<(), Response> {
    let usage = state.db.lock().unwrap().add_upload_limited(
        &upload.slug,
        &upload.name,
        bytes,
        created,
        (state.limits.gallery_bytes, state.limits.gallery_photos),
    );
    match usage {
        Ok(StoreUpload::Stored(..)) => {
            budget.unindexed.remove(&upload.name);
            budget
                .active
                .get_mut(&upload.slot)
                .expect("registered writer")
                .bytes = 0;
            Ok(())
        }
        Ok(StoreUpload::Missing) => Err(err(StatusCode::NOT_FOUND, "unknown gallery")),
        Ok(StoreUpload::Full) => Err(gallery_full()),
        Ok(StoreUpload::PhotoLimit) => {
            Err(err(StatusCode::PAYLOAD_TOO_LARGE, "photo limit reached"))
        }
        Err(_) => Err(ApiError::Internal.into_response()),
    }
}

// --- photo serve / delete ---

pub async fn get_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    serve_photo(state, addr, headers, name, false).await
}

/// JPEG preview from `<photos>/.thumbs`. The URL is `<stored_name>.jpg`, so
/// the normal `/photos/` media cookie authorises it. Missing = 404 (the
/// client then falls back to the original).
pub async fn get_thumb(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    serve_photo(state, addr, headers, name, true).await
}

/// Shared photo/thumbnail serving: auth, visibility, lock, rate limit,
/// caching, ETag and ranges are identical for both.
async fn serve_photo(
    state: Arc<AppState>,
    addr: std::net::SocketAddr,
    headers: HeaderMap,
    name: String,
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
    let key = request_key(&headers);
    let (access, key_ok) = {
        let db = state.db.lock().unwrap();
        let access = db.gallery_access(&slug);
        let key_ok = match key.as_ref() {
            Some(k) => db.key_owns_slug(k, &slug),
            None => Ok(false),
        };
        (access, key_ok)
    };
    let key_ok = match key_ok {
        Ok(ok) => ok,
        Err(_) => return ApiError::Internal.into_response(),
    };
    let access = match access {
        Ok(Some(access)) => access,
        Ok(None) => return missing_read(&state, &headers, &ip, "not found"),
        Err(_) => return ApiError::Internal.into_response(),
    };
    // Header pass stays accepted (old shared links may still send it); a
    // successful unlock sets a viewer cookie so later media needs neither.
    let owner = key_ok || media_cookie_ok(&headers, &state, &slug, MediaToken::Owner, 0);
    if !access.public && !owner {
        return missing_read(&state, &headers, &ip, "not found");
    }
    let unlocked = if owner {
        false
    } else {
        let pass = request_pass(&headers);
        match check_viewer(&state, &headers, ip, &slug, pass, &access).await {
            Ok(unlocked) => unlocked,
            Err(deny) => return deny,
        }
    };
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
        let ctype =
            if ext == ".ogg" || ext == ".oga" || ext == ".ogv" || ext == ".ogm" || ext == ".opus" {
                sniff_ogg(&canon_file)
                    .await
                    .unwrap_or_else(|| content_type_for(ext.as_str()))
            } else {
                content_type_for(ext.as_str())
            };
        (ext, ctype.to_string())
    };
    let range = parse_range(&headers, len);
    if let Some(Ok((start, _))) = range
        && file.seek(std::io::SeekFrom::Start(start)).await.is_err()
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
    }
    {
        let db = state.db.lock().unwrap();
        if let Err(deny) = current_access(&db, &slug, &access, owner, "not found") {
            return deny;
        }
        let stored = if thumb {
            name.strip_suffix(".jpg").unwrap_or_default()
        } else {
            &name
        };
        match db.photo_size(&slug, stored) {
            Ok(Some(_)) => {}
            Ok(None) => return err(StatusCode::NOT_FOUND, "not found"),
            Err(_) => return ApiError::Internal.into_response(),
        }
    }
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
        .header(header::CACHE_CONTROL, "private, no-store")
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
            issue_media_token(&state, &slug, MediaToken::Viewer, access.version),
        );
    }
    match range {
        // Unsatisfiable: 416 with the real length, no body (RFC 9110 §15.5.17).
        Some(Err(())) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap(),
        Some(Ok((start, end))) => {
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

fn deleted_photo(name: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "deleted": name, "gallery_gone": false })),
    )
        .into_response()
}

pub async fn delete_photo(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let Some(slug) = photo_delete_slug(&name).map(str::to_owned) else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    if let Some(denial) = owner_delete_denial(&state, &headers, addr, &slug) {
        return denial;
    }
    delete_photo_file(state, name, slug).await
}

pub async fn admin_delete_photo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if !is_admin(&headers, &state) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let Some(slug) = photo_delete_slug(&name).map(str::to_owned) else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    delete_photo_file(state, name, slug).await
}

fn photo_delete_slug(name: &str) -> Option<&str> {
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    photo_slug(name)
}

fn owner_delete_denial(
    state: &AppState,
    headers: &HeaderMap,
    addr: std::net::SocketAddr,
    slug: &str,
) -> Option<Response> {
    let owned = match request_key(headers) {
        Some(key) => state.db.lock().unwrap().key_owns_slug(&key, slug),
        None => Ok(false),
    };
    match owned {
        Ok(true) => None,
        Ok(false) => {
            // Missing and not-yours must look the same.
            let ip = client_ip(headers, Some(ConnectInfo(addr)));
            Some(if pass_fail_record(state, &ip) {
                ApiError::TooMany.into_response()
            } else {
                err(StatusCode::NOT_FOUND, "not found")
            })
        }
        Err(_) => Some(ApiError::Internal.into_response()),
    }
}

async fn delete_photo_file(state: Arc<AppState>, name: String, slug: String) -> Response {
    // A disconnected request must not release the admission lock mid-rename.
    match tokio::spawn(delete_photo_journaled(state, name, slug)).await {
        Ok(response) => response,
        Err(_) => ApiError::Internal.into_response(),
    }
}

async fn original_size(path: &Path) -> std::io::Result<Option<u64>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(md) if md.is_file() => Ok(Some(md.len())),
        Ok(_) => Err(std::io::Error::other("original is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

async fn sync_delete_dir(state: &Arc<AppState>) -> bool {
    let state = Arc::clone(state);
    matches!(
        tokio::task::spawn_blocking(move || crate::db::sync_dir(&state.dir)).await,
        Ok(Ok(()))
    )
}

async fn delete_photo_journaled(state: Arc<AppState>, name: String, slug: String) -> Response {
    // Exclude concurrent upload finalization and whole-gallery deletes.
    let mut budget = state.upload_budget.lock().await;
    let dir = &state.dir;
    let path = dir.join(&name);
    let pending = dir.join(format!("{}{name}", crate::db::PENDING_DELETE_PREFIX));
    if !matches!(tokio::fs::canonicalize(dir).await, Ok(path) if path == state.canon_dir) {
        return ApiError::Internal.into_response();
    }
    let (size, parked) = match (original_size(&path).await, original_size(&pending).await) {
        (Ok(Some(_)), Ok(Some(_))) => return ApiError::Internal.into_response(),
        (Ok(size), Ok(pending)) => (size, pending.is_some()),
        _ => return ApiError::Internal.into_response(),
    };
    let mut indexed = match state.db.lock().unwrap().photo_size(&slug, &name) {
        Ok(row) => row.is_some(),
        Err(_) => return ApiError::Internal.into_response(),
    };
    if let Some(size) = size
        && !indexed
    {
        // A preserved upload needs a durable row before it can be parked, or
        // recovery cannot tell a failed delete from a committed one.
        if !sync_delete_dir(&state).await {
            return ApiError::Internal.into_response();
        }
        let mut db = state.db.lock().unwrap();
        match db.add_upload(&slug, &name, size, crate::db::now_millis()) {
            Ok(Some(_)) if matches!(db.photo_size(&slug, &name), Ok(Some(_))) => {
                budget.unindexed.remove(&name);
                indexed = true;
            }
            Ok(None) => return err(StatusCode::NOT_FOUND, "not found"),
            _ => return ApiError::Internal.into_response(),
        }
    }
    let had_original = size.is_some() || parked;
    if size.is_some() && tokio::fs::rename(&path, &pending).await.is_err() {
        return ApiError::Internal.into_response();
    }
    // Make the journal durable before forgetting a row, also on retry.
    if had_original && !sync_delete_dir(&state).await {
        return ApiError::Internal.into_response();
    }
    if indexed {
        let mut db = state.db.lock().unwrap();
        if db.remove_upload(&slug, &name, 0).is_err()
            || !matches!(db.photo_size(&slug, &name), Ok(None))
        {
            return ApiError::Internal.into_response();
        }
    } else if !had_original {
        budget.unindexed.remove(&name);
        return err(StatusCode::NOT_FOUND, "not found");
    }
    if had_original && tokio::fs::remove_file(&pending).await.is_err() {
        return ApiError::Internal.into_response();
    }
    budget.unindexed.remove(&name);
    let _ = tokio::fs::remove_file(dir.join(".thumbs").join(format!("{name}.jpg"))).await;
    deleted_photo(&name)
}

pub async fn fallback_404() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_viewer_check_keeps_its_slot_and_records_failure() {
        use crate::state::{Limits, PASS_FAIL_MAX};

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let state = AppState::new_with(
                    root.path(),
                    Limits {
                        argon2_slots: 1,
                        ..Limits::defaults()
                    },
                )
                .await
                .unwrap();
                let headers = HeaderMap::new();
                let access = GalleryAccess {
                    public: true,
                    hash: Some(crate::db::hash_password("viewer-password")),
                    version: 0,
                };
                let ip = IpAddr::from([127, 0, 0, 1]);
                for _ in 1..PASS_FAIL_MAX {
                    assert!(!pass_fail_record(&state, &ip));
                }
                let (release, gate) = tokio::sync::oneshot::channel::<()>();
                let (started, running) = tokio::sync::oneshot::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    let _ = gate.blocking_recv();
                });
                running.await.unwrap();

                let check = |ip| {
                    check_viewer(
                        &state,
                        &headers,
                        ip,
                        "test-gallery-1",
                        "wrong-password".into(),
                        &access,
                    )
                };
                let mut request = Box::pin(check(ip));
                assert!(futures::poll!(&mut request).is_pending());
                drop(request);
                assert!(pass_fail_check(&state, ip, || true).is_none());
                let other_ip = IpAddr::from([127, 0, 0, 2]);
                let mut other = std::pin::pin!(check(other_ip));
                let std::task::Poll::Ready(Err(denied)) = futures::poll!(&mut other) else {
                    panic!("a full hash pool must refuse immediately");
                };
                assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);

                release.send(()).unwrap();
                blocker.await.unwrap();
                let _permit = tokio::time::timeout(Duration::from_secs(10), state.argon2.acquire())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(pass_fail_check(&state, ip, || true).is_none());
            });
    }

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
        headers.insert("x-forwarded-for", "192.0.2.5".parse().unwrap());
        headers.append("x-forwarded-for", "198.51.100.7, 10.9.9.9".parse().unwrap());
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
    fn metrics_requires_unproxied_loopback() {
        let trusted = parse_trust_proxy("127.0.0.1, 172.17.0.1");
        let loopback = "127.0.0.1".parse().unwrap();
        let bridge = "172.17.0.1".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert!(metrics_direct_peer(loopback, &headers, &[]));
        assert!(!metrics_direct_peer(loopback, &headers, &trusted));
        assert!(!metrics_direct_peer(bridge, &headers, &trusted));
        for value in ["bad-forwarding", "127.0.0.1"] {
            headers.insert("x-forwarded-for", value.parse().unwrap());
            assert!(!metrics_direct_peer(loopback, &headers, &[]));
            assert!(!metrics_direct_peer(loopback, &headers, &trusted));
        }
        headers.clear();
        headers.insert("forwarded", "for=127.0.0.1".parse().unwrap());
        assert!(!metrics_direct_peer(loopback, &headers, &[]));
    }

    #[tokio::test]
    async fn upload_slots_are_free_while_thumbnail_runs() {
        use std::sync::atomic::Ordering;

        // Disarm even if an assertion panics, so another upload test cannot hang.
        struct Disarm;
        impl Drop for Disarm {
            fn drop(&mut self) {
                THUMB_GATE.armed.store(false, Ordering::SeqCst);
                THUMB_GATE.release.add_permits(1);
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = AppState::new_with(
            tmp.path(),
            crate::state::Limits {
                upload_slots: 1,
                upload_slots_per_ip: 1,
                ..crate::state::Limits::defaults()
            },
        )
        .await
        .expect("state");
        let slug = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("slot-key-0001")
            .expect("gallery");

        THUMB_GATE.armed.store(true, Ordering::SeqCst);
        let _disarm = Disarm;

        let mut headers = HeaderMap::new();
        headers.insert("x-gallery-key", "slot-key-0001".parse().unwrap());
        headers.insert("x-gallery-slug", slug.parse().unwrap());
        let response = tokio::spawn({
            let state = state.clone();
            let headers = headers.clone();
            async move {
                put_photo(
                    State(state),
                    ConnectInfo("127.0.0.1:4242".parse().unwrap()),
                    headers,
                    AxumPath("a.png".to_string()),
                    Body::from("x"),
                )
                .await
            }
        });

        // `write_thumb` is parked: streaming and indexing are done, so both
        // upload permits must already be back.
        let wait = Duration::from_secs(2);
        let _entered = tokio::time::timeout(wait, THUMB_GATE.entered.acquire())
            .await
            .expect("write_thumb reached")
            .unwrap();
        assert_eq!(state.uploads.available_permits(), 1);
        assert!(state.upload_ips.lock().unwrap().is_empty());

        {
            let _other_thumb = state.thumbs.try_acquire().expect("second thumbnail slot");
            let second = tokio::time::timeout(
                wait,
                put_photo(
                    State(state.clone()),
                    ConnectInfo("127.0.0.1:4242".parse().unwrap()),
                    headers,
                    AxumPath("b.png".to_string()),
                    Body::from("second original"),
                ),
            )
            .await
            .expect("thumbnail saturation must not delay a new upload");
            assert_eq!(second.status(), StatusCode::CREATED);
            let body = axum::body::to_bytes(second.into_body(), 4096)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let name = json["name"].as_str().unwrap();
            assert_eq!(
                std::fs::read(state.dir.join(name)).unwrap(),
                b"second original"
            );
        }

        THUMB_GATE.release.add_permits(1);
        let response = tokio::time::timeout(wait, response)
            .await
            .expect("thumbnail released")
            .expect("join");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(state.uploads.available_permits(), 1);
        assert!(state.upload_ips.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_orphans_keeps_files_on_db_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(tmp.path()).await.expect("state");
        let slug = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("sweep-key-0001")
            .expect("gallery");
        let name = format!("{slug}_{}_x.jpg", "a".repeat(32));
        std::fs::write(state.dir.join(&name), b"photo").unwrap();
        state
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE galleries")
            .unwrap();

        let mut deleted = 0;
        let mut failed = 0;
        let result = sweep_orphans(
            &state,
            &state.dir,
            &std::collections::HashSet::new(),
            true,
            &mut deleted,
            &mut failed,
        )
        .await;
        assert!(result.is_err(), "a DB read error must abort the sweep");
        assert_eq!((deleted, failed), (0, 0));
        assert!(
            state.dir.join(&name).exists(),
            "a live photo must not be deleted on a failed lookup"
        );
    }

    #[tokio::test]
    async fn db_errors_are_500_not_unlocked_or_404() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(tmp.path()).await.expect("state");
        let slug = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("dberr-key-0001")
            .expect("gallery");
        state
            .db
            .lock()
            .unwrap()
            .execute("UPDATE galleries SET public = 1 WHERE slug = ?1", [&slug])
            .unwrap();
        let name = format!("{slug}_{}_x.jpg", "b".repeat(32));
        std::fs::write(state.dir.join(&name), b"photo").unwrap();

        // A failed viewer-cookie version read must not be guessed as version 0.
        state
            .db
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE galleries RENAME COLUMN token_version TO token_version_off")
            .unwrap();
        let listing = gallery_photos(
            State(state.clone()),
            ConnectInfo("127.0.0.1:4242".parse().unwrap()),
            HeaderMap::new(),
            AxumPath(slug.clone()),
            axum::extract::RawQuery(None),
        )
        .await;
        assert_eq!(listing.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let media = get_photo(
            State(state.clone()),
            ConnectInfo("127.0.0.1:4242".parse().unwrap()),
            HeaderMap::new(),
            AxumPath(name.clone()),
        )
        .await;
        assert_eq!(media.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Keep the gallery row readable but break the lock read.
        state
            .db
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE galleries RENAME COLUMN pass_hash TO pass_hash_off")
            .unwrap();
        let listing = gallery_photos(
            State(state.clone()),
            ConnectInfo("127.0.0.1:4242".parse().unwrap()),
            HeaderMap::new(),
            AxumPath(slug.clone()),
            axum::extract::RawQuery(None),
        )
        .await;
        assert_eq!(listing.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let media = get_photo(
            State(state.clone()),
            ConnectInfo("127.0.0.1:4242".parse().unwrap()),
            HeaderMap::new(),
            AxumPath(name),
        )
        .await;
        assert_eq!(media.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Break the gallery lookup itself: still a server error, not 404.
        state
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE galleries")
            .unwrap();
        let listing = gallery_photos(
            State(state.clone()),
            ConnectInfo("127.0.0.1:4242".parse().unwrap()),
            HeaderMap::new(),
            AxumPath(slug),
            axum::extract::RawQuery(None),
        )
        .await;
        assert_eq!(listing.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn non_utf8_slug_header_is_a_bad_request_not_a_mint() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-gallery-slug",
            HeaderValue::from_bytes(b"\xff").expect("obs-text header"),
        );
        assert!(request_slug(&headers).is_err());
        assert!(matches!(request_slug(&HeaderMap::new()), Ok(None)));
    }

    #[tokio::test]
    async fn part_sweeper_reaps_expired_parts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(tmp.path()).await.expect("state");
        // Startup already swept; create a stale part afterwards.
        let stale = state.dir.join(".part-expired-upload");
        std::fs::write(&stale, b"stale").expect("write part");
        let old = std::time::SystemTime::now() - Duration::from_secs(PART_TTL.as_secs() + 60);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .expect("open part");
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("age part");
        drop(file);

        let task = tokio::spawn(part_sweeper(state.clone(), Duration::from_millis(20)));
        let reaped = tokio::time::timeout(Duration::from_secs(2), async {
            while stale.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        task.abort();
        let _ = task.await;
        assert!(
            reaped.is_ok(),
            "part sweeper did not remove the expired temp"
        );
    }

    #[tokio::test]
    async fn empty_gallery_sweep_keeps_recent_or_uploaded_galleries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(tmp.path()).await.expect("state");
        let old = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("empty-old-key-01")
            .expect("old");
        let recent = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("empty-recent-key")
            .expect("recent");
        let with_file = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("empty-file-key-01")
            .expect("file");
        let active = state
            .db
            .lock()
            .unwrap()
            .get_or_create_gallery("empty-active-key")
            .expect("active");
        let (emptied, legacy, preview_only, with_part, with_pending) = {
            let mut db = state.db.lock().unwrap();
            let emptied = db.get_or_create_gallery("emptied-url-key-01").unwrap();
            db.add_upload(&emptied, "stored.jpg", 3, 0)
                .unwrap()
                .unwrap();
            db.remove_upload(&emptied, "stored.jpg", 3)
                .unwrap()
                .unwrap();
            let legacy = db.get_or_create_gallery("legacy-empty-key-01").unwrap();
            db.execute(
                "UPDATE galleries SET ever_stored = NULL WHERE slug = ?1",
                [&legacy],
            )
            .unwrap();
            let preview_only = db.get_or_create_gallery("preview-only-key").unwrap();
            let with_part = db.get_or_create_gallery("empty-part-key").unwrap();
            let with_pending = db.get_or_create_gallery("empty-pending-key").unwrap();
            (emptied, legacy, preview_only, with_part, with_pending)
        };
        let cutoff = crate::db::now_millis() / 1000 - EMPTY_GALLERY_TTL.as_secs() as i64 - 60;
        for slug in [
            &old,
            &with_file,
            &active,
            &emptied,
            &legacy,
            &preview_only,
            &with_part,
            &with_pending,
        ] {
            state
                .db
                .lock()
                .unwrap()
                .execute(
                    "UPDATE galleries SET created = ?1 WHERE slug = ?2",
                    rusqlite::params![cutoff, slug],
                )
                .unwrap();
        }
        let stray = format!("{with_file}_{}_x.jpg", "a".repeat(32));
        std::fs::write(state.dir.join(&stray), b"x").unwrap();
        let part = state.dir.join(format!(".part-{with_part}_unusual"));
        let pending = state.dir.join(format!(
            "{}{with_pending}_",
            crate::db::PENDING_DELETE_PREFIX
        ));
        std::fs::write(&part, b"part").unwrap();
        std::fs::create_dir(&pending).unwrap();
        let thumbs = state.dir.join(".thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        let stale = thumbs.join(format!("{preview_only}_stale.jpg"));
        let kept = thumbs.join(format!("{with_file}_kept.jpg"));
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&kept, b"kept").unwrap();
        state
            .upload_budget
            .lock()
            .await
            .active
            .insert(format!("{active}_{}", "b".repeat(32)), Default::default());

        let removed = sweep_empty_galleries(&state).await.expect("sweep");
        assert_eq!(removed, 2);
        assert!(!stale.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), b"kept");
        assert_eq!(std::fs::read(state.dir.join(&stray)).unwrap(), b"x");
        assert_eq!(std::fs::read(&part).unwrap(), b"part");
        assert!(pending.is_dir());
        let db = state.db.lock().unwrap();
        assert!(!db.gallery_exists(&old).unwrap());
        assert!(!db.gallery_exists(&preview_only).unwrap());
        assert!(db.gallery_exists(&with_part).unwrap());
        assert!(db.gallery_exists(&with_pending).unwrap());
        assert!(db.gallery_exists(&recent).unwrap());
        assert!(db.gallery_exists(&with_file).unwrap());
        assert!(db.gallery_exists(&active).unwrap());
        assert!(db.gallery_exists(&emptied).unwrap());
        assert!(db.gallery_exists(&legacy).unwrap());
    }

    #[test]
    fn upload_floor_rejects_slow_windows_but_never_a_complete_frame() {
        let start = Instant::now();
        let sec = Duration::from_secs;

        // Inside the first window any amount is fine, even one byte.
        let mut short = UploadFloor::new(start, None);
        assert!(short.record(start + sec(59), 1, false));

        // A window that closes under the floor aborts an incomplete body.
        let mut slow = UploadFloor::new(start, None);
        assert!(!slow.record(
            start + UPLOAD_FLOOR_WINDOW,
            MIN_UPLOAD_BYTES_PER_WINDOW - 1,
            false
        ));

        // The frame that completes the declared body is always accepted.
        let mut done = UploadFloor::new(start, None);
        assert!(done.record(start + UPLOAD_FLOOR_WINDOW, 10, true));

        // Meeting the floor resets the window: only its own bytes count.
        let mut fast = UploadFloor::new(start, None);
        assert!(fast.record(
            start + UPLOAD_FLOOR_WINDOW,
            MIN_UPLOAD_BYTES_PER_WINDOW,
            false
        ));
        assert!(fast.record(start + sec(119), 1, false));
        assert!(!fast.record(start + sec(120), 1, false));

        // A small declared body is judged against its own size, not 8 KiB, so
        // a legitimate small resumable chunk still finishes slowly.
        let mut small = UploadFloor::new(start, Some(1024));
        assert!(small.record(start + UPLOAD_FLOOR_WINDOW, 1024, false));
        let mut tiny = UploadFloor::new(start, Some(512));
        assert!(!tiny.record(start + UPLOAD_FLOOR_WINDOW, 511, false));
    }
}
