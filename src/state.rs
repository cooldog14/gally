use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::db;

pub const RATE_WINDOW_SECS: u64 = 60;

/// How long a verified (hash, password) pair stays accepted.
pub const PASS_CACHE_TTL: Duration = Duration::from_secs(300);
/// Failed password/key attempts allowed per IP per window.
pub const PASS_FAIL_MAX: usize = 10;
pub const PASS_FAIL_WINDOW_SECS: u64 = 60;

// (Listing cache removed: plain directory scans never go stale.)

/// Upload/read limits, snapshotted at startup so hot paths never read env.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Single request cap (`GALLY_MAX_BYTES`).
    pub max_file: usize,
    /// Per-gallery byte cap (`GALLY_GALLERY_MAX_BYTES`); 0 = off.
    pub gallery_bytes: u64,
    /// Per-gallery photo cap (`GALLY_GALLERY_MAX_PHOTOS`); 0 = off.
    pub gallery_photos: i64,
    /// Free space the data volume must keep (`GALLY_MIN_FREE_BYTES`); 0 = off.
    pub min_free: u64,
    /// Uploads per IP per window (`GALLY_RATE_MAX`); 0 = off.
    pub rate_max: usize,
    /// Listing + photo reads per IP per window (`GALLY_READ_RATE_MAX`);
    /// 0 = off. Keep it above `gallery_photos`: one gallery view costs one
    /// listing plus one request per photo, all from the same bucket.
    pub read_max: usize,
    /// Concurrent uploads (`GALLY_UPLOAD_SLOTS`), clamped to >= 1.
    pub upload_slots: usize,
    /// Concurrent uploads per IP (`GALLY_UPLOAD_SLOTS_PER_IP`), clamped to >= 1.
    /// Sits alongside the global semaphore: one slow client cannot pin all
    /// 32 global slots while everyone else gets 503.
    pub upload_slots_per_ip: usize,
    /// Concurrent Argon2 hashes (`GALLY_ARGON2_SLOTS`), clamped to >= 1.
    pub argon2_slots: usize,
    /// Append `Secure` to login and media cookies (`GALLY_SECURE_COOKIES`).
    pub secure_cookies: bool,
}

impl Limits {
    pub fn defaults() -> Self {
        Limits {
            max_file: 10 * 1024 * 1024 * 1024,
            gallery_bytes: 5 * 1024 * 1024 * 1024,
            gallery_photos: 5000,
            min_free: 2 * 1024 * 1024 * 1024,
            rate_max: 60,
            read_max: 12_000,
            upload_slots: 32,
            upload_slots_per_ip: 4,
            argon2_slots: 4,
            secure_cookies: false,
        }
    }

    pub fn from_env() -> Self {
        let d = Limits::defaults();
        let num = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let flag = |name: &str| {
            matches!(
                std::env::var(name)
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .as_str(),
                "1" | "true" | "yes"
            )
        };
        Limits {
            max_file: num("GALLY_MAX_BYTES").unwrap_or(d.max_file as u64) as usize,
            gallery_bytes: num("GALLY_GALLERY_MAX_BYTES").unwrap_or(d.gallery_bytes),
            gallery_photos: num("GALLY_GALLERY_MAX_PHOTOS").unwrap_or(d.gallery_photos as u64)
                as i64,
            min_free: num("GALLY_MIN_FREE_BYTES").unwrap_or(d.min_free),
            rate_max: num("GALLY_RATE_MAX").unwrap_or(d.rate_max as u64) as usize,
            read_max: num("GALLY_READ_RATE_MAX").unwrap_or(d.read_max as u64) as usize,
            upload_slots: (num("GALLY_UPLOAD_SLOTS").unwrap_or(d.upload_slots as u64) as usize)
                .max(1),
            upload_slots_per_ip: (num("GALLY_UPLOAD_SLOTS_PER_IP")
                .unwrap_or(d.upload_slots_per_ip as u64)
                as usize)
                .max(1),
            argon2_slots: (num("GALLY_ARGON2_SLOTS").unwrap_or(d.argon2_slots as u64) as usize)
                .max(1),
            secure_cookies: flag("GALLY_SECURE_COOKIES"),
        }
    }
}

pub struct AppState {
    pub dir: PathBuf,
    pub canon_dir: PathBuf,
    pub db: Mutex<rusqlite::Connection>,
    pub admin_tokens: Mutex<HashMap<String, Instant>>,
    pub rate: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    /// Verified (stored hash, supplied password) digests: a locked gallery
    /// costs one Argon2 run, not one per image.
    pub pass_ok: Mutex<HashMap<Vec<u8>, Instant>>,
    /// Failed password/key attempts per IP (sliding window).
    pub pass_fail: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    pub statics: HashMap<&'static str, (&'static [u8], &'static str, String)>,
    /// Invite code gating NEW gallery creation (`GALLY_INVITE_CODE` at
    /// startup; `None` = open creation). Contributing to an existing
    /// gallery never needs it.
    pub invite: Mutex<Option<String>>,
    pub limits: Limits,
    /// HMAC key for stateless media cookies (`GALLY_MEDIA_SECRET`, else the
    /// generated `<root>/.gally-media-secret`).
    pub media_secret: Vec<u8>,
    pub uploads: tokio::sync::Semaphore,
    /// Live upload streams per IP. Guarded by `UploadSlotGuard`: a slow
    /// client pins at most `limits.upload_slots_per_ip` global slots.
    pub upload_ips: Mutex<HashMap<IpAddr, usize>>,
    pub argon2: tokio::sync::Semaphore,
    /// Thumbnail decodes in flight. Hardcoded to 2: a small thumbnail can
    /// need the full source bitmap in RAM, so this bounds peak decode memory.
    pub thumbs: tokio::sync::Semaphore,
    /// Read requests per IP (listing + photo), separate from uploads.
    pub read: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    /// Exclusive data-dir lock: a second server must never sweep the first
    /// instance's `.part-` uploads. Held until the state is dropped.
    pub _instance_lock: Option<std::fs::File>,
    pub started: Instant,
    pub requests_total: AtomicU64,
    pub responses_5xx_total: AtomicU64,
    pub uploads_total: AtomicU64,
    pub upload_bytes_total: AtomicU64,
}

/// Non-blocking exclusive flock on `<root>/.gally.lock`; `None` off unix.
fn instance_lock(root: &Path) -> anyhow::Result<Option<std::fs::File>> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let path = root.join(".gally.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("open {}: {e}", path.display()))?;
        // SAFETY: `file` owns the fd for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            anyhow::bail!(
                "{} is already in use by another gally instance",
                root.display()
            );
        }
        Ok(Some(file))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(None)
    }
}

/// Lowercase hex, no external crate.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// HMAC key for media cookies: `GALLY_MEDIA_SECRET`, else a 32-byte secret
/// generated once into `<root>/.gally-media-secret` (0600) and reused.
fn media_secret(root: &Path) -> anyhow::Result<Vec<u8>> {
    if let Ok(s) = std::env::var("GALLY_MEDIA_SECRET")
        && !s.is_empty()
    {
        return Ok(s.into_bytes());
    }
    let path = root.join(".gally-media-secret");
    match std::fs::read(&path) {
        Ok(raw) => {
            let text = String::from_utf8_lossy(&raw);
            if let Some(bytes) = from_hex(text.trim())
                && bytes.len() == 32
            {
                return Ok(bytes);
            }
            // A crash or full disk can leave an empty/partial file here.
            // Rewriting it only invalidates old media cookies.
            tracing::warn!(
                path = %path.display(),
                "invalid media secret; generating a new one"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    }
    let mut bytes = [0u8; 32];
    {
        use rand_core::RngCore;
        rand_core::OsRng.fill_bytes(&mut bytes);
    }
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    // The instance lock is held: no other process races this truncate.
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` above only applies to a new file; a rewritten one may be 0644.
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(to_hex(&bytes).as_bytes())?;
    file.sync_all()?;
    Ok(bytes.to_vec())
}

fn hmac_sha256(secret: &[u8], msg: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AppState {
    pub async fn new(root: &Path) -> anyhow::Result<Arc<AppState>> {
        Self::new_with(root, Limits::from_env()).await
    }

    pub async fn new_with(root: &Path, limits: Limits) -> anyhow::Result<Arc<AppState>> {
        // Lock before touching the dir: the stale-.part sweep below would
        // otherwise delete a live server's in-flight uploads.
        tokio::fs::create_dir_all(root).await?;
        let _instance_lock = instance_lock(root)?;
        // After the flock: two instances must never race the secret file.
        let media_secret = media_secret(root)?;
        let dir = root.join("photos");
        tokio::fs::create_dir_all(&dir).await?;
        // Data dir holds every uploaded file and the SQLite file: owner-only.
        // A host bind mount must already be owned by the container uid (10001);
        // the shipped compose file uses a named volume to inherit that.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [root, dir.as_path()] {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            }
        }
        // An upload that died mid-stream leaves a `.part-` temp behind; they
        // are never listed, so drop them here rather than let them pile up.
        if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                if ent.file_name().to_string_lossy().starts_with(".part-") {
                    let _ = tokio::fs::remove_file(ent.path()).await;
                }
            }
        }
        // Thumbnail writes are temp+rename; a crash can leave `.tmp-*` behind.
        if let Ok(mut rd) = tokio::fs::read_dir(dir.join(".thumbs")).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                if ent.file_name().to_string_lossy().starts_with(".tmp-") {
                    let _ = tokio::fs::remove_file(ent.path()).await;
                }
            }
        }
        // db::open + recount are synchronous but fast (local SQLite); run
        // them blocking so we never stall the runtime.
        let db_path = root.join("galleries.db");
        let scan_dir = dir.clone();
        let conn = tokio::task::spawn_blocking(move || {
            let mut conn = db::open(&db_path)?;
            db::recount(&mut conn, &scan_dir)?;
            let backfilled = db::backfill_exif(&mut conn, &scan_dir)?;
            if backfilled > 0 {
                tracing::info!(photos = backfilled, "EXIF capture times backfilled");
            }
            Ok::<_, anyhow::Error>(conn)
        })
        .await??;
        let statics: HashMap<&'static str, (&'static [u8], &'static str)> = HashMap::from([
            ("/", (&include_bytes!("../index.html")[..], "text/html")),
            (
                "/index.html",
                (&include_bytes!("../index.html")[..], "text/html"),
            ),
            (
                "/app.js",
                (&include_bytes!("../app.js")[..], "application/javascript"),
            ),
            (
                "/vendor-heic2any.min.js",
                (
                    &include_bytes!("../vendor-heic2any.min.js")[..],
                    "application/javascript",
                ),
            ),
            (
                "/styles.css",
                (&include_bytes!("../styles.css")[..], "text/css"),
            ),
            (
                "/fonts/Figtree-400.woff2",
                (
                    &include_bytes!("../fonts/Figtree-400.woff2")[..],
                    "font/woff2",
                ),
            ),
            (
                "/fonts/Figtree-500.woff2",
                (
                    &include_bytes!("../fonts/Figtree-500.woff2")[..],
                    "font/woff2",
                ),
            ),
            (
                "/fonts/Figtree-600.woff2",
                (
                    &include_bytes!("../fonts/Figtree-600.woff2")[..],
                    "font/woff2",
                ),
            ),
        ]);
        let statics = statics
            .into_iter()
            .map(|(path, (bytes, ctype))| {
                use sha2::{Digest, Sha256};
                let hex: String = Sha256::digest(bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                (path, (bytes, ctype, format!("\"{hex}\"")))
            })
            .collect();
        Ok(Arc::new(AppState {
            canon_dir: dir.canonicalize().unwrap_or_else(|_| dir.clone()),
            dir,
            db: Mutex::new(conn),
            admin_tokens: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
            pass_ok: Mutex::new(HashMap::new()),
            pass_fail: Mutex::new(HashMap::new()),
            statics,
            invite: Mutex::new(
                std::env::var("GALLY_INVITE_CODE")
                    .ok()
                    .filter(|s| !s.trim().is_empty()),
            ),
            limits,
            media_secret,
            uploads: tokio::sync::Semaphore::new(limits.upload_slots),
            upload_ips: Mutex::new(HashMap::new()),
            argon2: tokio::sync::Semaphore::new(limits.argon2_slots),
            thumbs: tokio::sync::Semaphore::new(2),
            read: Mutex::new(HashMap::new()),
            _instance_lock,
            started: Instant::now(),
            requests_total: AtomicU64::new(0),
            responses_5xx_total: AtomicU64::new(0),
            uploads_total: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
        }))
    }
}

/// RAII guard for one live upload slot from `ip`: `try_acquire` refuses when
/// that IP already holds `cap` concurrent uploads, otherwise the count bumps
/// and `Drop` releases it. The global `uploads` semaphore is acquired
/// separately (first), so ordering is fixed: global, then per-IP.
pub struct UploadSlotGuard<'a> {
    ips: &'a Mutex<HashMap<IpAddr, usize>>,
    ip: IpAddr,
}

impl<'a> UploadSlotGuard<'a> {
    /// `cap` of 0 means unlimited (direct callers only; from_env clamps to >= 1).
    pub fn try_acquire(
        ips: &'a Mutex<HashMap<IpAddr, usize>>,
        ip: &IpAddr,
        cap: usize,
    ) -> Option<Self> {
        if cap > 0 {
            let mut guard = ips.lock().unwrap();
            let n = guard.get(ip).copied().unwrap_or(0);
            if n >= cap {
                return None;
            }
            guard.insert(*ip, n + 1);
        }
        Some(Self { ips, ip: *ip })
    }
}

impl Drop for UploadSlotGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self.ips.lock().unwrap();
        match guard.get(&self.ip).copied() {
            Some(n) if n > 1 => {
                guard.insert(self.ip, n - 1);
            }
            _ => {
                guard.remove(&self.ip);
            }
        }
    }
}

/// Fixed-window rate limit: at most `max` hits per window (0 = off).
/// Returns true when the caller is over the limit (hit NOT recorded).
/// shortcut: hard cap at 10k IPs, evicts oldest — switch to LRU when NAT-heavy.
pub fn window_hit(
    map: &Mutex<HashMap<IpAddr, Vec<Instant>>>,
    ip: &IpAddr,
    max: usize,
    window_secs: u64,
) -> bool {
    if max == 0 {
        return false;
    }
    let now = Instant::now();
    let mut hits_map = map.lock().unwrap();
    // Bound memory: drop the oldest entry when full (dead IPs never return).
    if !hits_map.contains_key(ip)
        && hits_map.len() >= 10_000
        && let Some(oldest) = hits_map
            .iter()
            .min_by_key(|(_, v)| v.first().copied())
            .map(|(k, _)| *k)
    {
        hits_map.remove(&oldest);
    }
    let hits = hits_map.entry(*ip).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs() < window_secs);
    if hits.len() >= max {
        return true;
    }
    hits.push(now);
    false
}

/// Upload throttle (per IP).
pub fn rate_limited(state: &AppState, ip: &IpAddr) -> bool {
    window_hit(&state.rate, ip, state.limits.rate_max, RATE_WINDOW_SECS)
}

/// Read throttle for listings and photo serving (per IP).
pub fn read_limited(state: &AppState, ip: &IpAddr) -> bool {
    window_hit(&state.read, ip, state.limits.read_max, RATE_WINDOW_SECS)
}

/// True when `ip` already burned `PASS_FAIL_MAX` attempts in the window.
/// Read-only: never records a failure (the caller still refuses outright).
pub fn pass_fail_exceeded(state: &AppState, ip: &IpAddr) -> bool {
    let now = Instant::now();
    let mut fail = state.pass_fail.lock().unwrap();
    let Some(hits) = fail.get_mut(ip) else {
        return false;
    };
    hits.retain(|t| now.duration_since(*t).as_secs() < PASS_FAIL_WINDOW_SECS);
    hits.len() >= PASS_FAIL_MAX
}

/// Record one failed password/key attempt. Returns true once over the limit.
/// shortcut: same 10k-IP bound as rate_limited, evicts oldest — switch to LRU
/// when NAT-heavy.
pub fn pass_fail_record(state: &AppState, ip: &IpAddr) -> bool {
    let now = Instant::now();
    let mut fail = state.pass_fail.lock().unwrap();
    if !fail.contains_key(ip)
        && fail.len() >= 10_000
        && let Some(oldest) = fail
            .iter()
            .min_by_key(|(_, v)| v.first().copied())
            .map(|(k, _)| *k)
    {
        fail.remove(&oldest);
    }
    let hits = fail.entry(*ip).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs() < PASS_FAIL_WINDOW_SECS);
    hits.push(now);
    hits.len() > PASS_FAIL_MAX
}

/// Admin tokens live 24h from issue; expired ones are dropped on sight.
pub const ADMIN_TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);

/// Owner and viewer media cookies outlive a browsing session but not a day.
pub const OWNER_TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);

/// What a media cookie authorises. The kind is part of the signed message,
/// so a viewer cookie can never act as an owner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaToken {
    Owner,
    Viewer,
}

fn media_message(slug: &str, kind: MediaToken, version: i64, exp: u64) -> String {
    match kind {
        MediaToken::Owner => format!("v1|{slug}|owner|{exp}"),
        // Revocation is per gallery: a password change bumps token_version.
        // Upgrade to per-session ids only if single-cookie logout is needed.
        MediaToken::Viewer => format!("v1|{slug}|viewer|{exp}|{version}"),
    }
}

/// `v1.<exp_unix>.<mac_hex>` for one media cookie.
pub fn media_token(secret: &[u8], slug: &str, kind: MediaToken, version: i64, exp: u64) -> String {
    let mac = hmac_sha256(secret, media_message(slug, kind, version, exp).as_bytes());
    format!("v1.{exp}.{}", to_hex(&mac))
}

/// True when `token` is live, signed for `slug`/`kind`, and (for viewers)
/// carries the gallery's current `version`.
pub fn media_token_ok(
    secret: &[u8],
    token: &str,
    slug: &str,
    kind: MediaToken,
    version: i64,
) -> bool {
    let mut parts = token.split('.');
    let (Some("v1"), Some(exp), Some(mac)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(exp) = exp.parse::<u64>() else {
        return false;
    };
    if exp <= now_secs() {
        return false;
    }
    let expected = to_hex(&hmac_sha256(
        secret,
        media_message(slug, kind, version, exp).as_bytes(),
    ));
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(mac.as_bytes()).unwrap_u8() == 1
}

/// Value of `name` in the request's `Cookie` header.
pub fn cookie_value<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
}

/// Raw `admin=` token, without validating it (logout needs it to revoke).
pub fn admin_token(headers: &axum::http::HeaderMap) -> Option<String> {
    cookie_value(headers, "admin").map(str::to_string)
}

/// True when the request carries a live `admin=` cookie value.
pub fn is_admin(headers: &axum::http::HeaderMap, state: &AppState) -> bool {
    let Some(tok) = admin_token(headers) else {
        return false;
    };
    let mut tokens = state.admin_tokens.lock().unwrap();
    match tokens.get(&tok) {
        Some(issued) if issued.elapsed() < ADMIN_TOKEN_TTL => true,
        // Expired (or just removed): drop it so the map never grows.
        _ => {
            tokens.remove(&tok);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One gallery view (listing + one request per photo) comes out of the
    /// same bucket: a cap above `gallery_photos` is what keeps a big gallery
    /// from 429-ing its own viewer halfway through.
    #[test]
    fn read_limit_covers_a_full_gallery() {
        let d = Limits::defaults();
        assert!(
            d.read_max as i64 > d.gallery_photos,
            "read_max {} must exceed gallery_photos {}",
            d.read_max,
            d.gallery_photos
        );
    }

    #[test]
    fn upload_slot_guard_caps_one_ip_and_releases() {
        let ips = Mutex::new(HashMap::new());
        let a: IpAddr = "127.0.0.1".parse().unwrap();
        let b: IpAddr = "127.0.0.2".parse().unwrap();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(UploadSlotGuard::try_acquire(&ips, &a, 4).expect("slot"));
        }
        assert!(UploadSlotGuard::try_acquire(&ips, &a, 4).is_none());
        // Another IP is unaffected.
        assert!(UploadSlotGuard::try_acquire(&ips, &b, 4).is_some());
        drop(held.pop());
        assert!(UploadSlotGuard::try_acquire(&ips, &a, 4).is_some());
    }

    #[test]
    fn invalid_media_secret_is_regenerated() {
        let dir = std::env::temp_dir().join(format!("gally-secret-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".gally-media-secret");
        std::fs::write(&path, b"").unwrap();
        // A crash/ENOSPC can leave this empty; startup must still work.
        let secret = media_secret(&dir).expect("regenerate the empty secret");
        assert_eq!(secret.len(), 32);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().len(), 64, "rewritten as 32-byte hex");
        assert_eq!(media_secret(&dir).expect("reuse"), secret);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
