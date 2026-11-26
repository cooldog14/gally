use std::collections::{HashMap, HashSet};
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
    /// Resumable-chunk requests per IP per window (`GALLY_CHUNK_RATE_MAX`);
    /// 0 = off. Separate bucket: one file is many requests.
    pub chunk_max: usize,
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
            chunk_max: 600,
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
            chunk_max: num("GALLY_CHUNK_RATE_MAX").unwrap_or(d.chunk_max as u64) as usize,
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
    pub rate: Mutex<HashMap<IpAddr, WindowRing>>,
    /// Verified (stored hash, supplied password) digests: a locked gallery
    /// costs one Argon2 run, not one per image.
    pub pass_ok: Mutex<HashMap<Vec<u8>, Instant>>,
    /// Failed and in-flight credential attempts share one budget per IP.
    pass_fail: Mutex<HashMap<IpAddr, CredentialAttempts>>,
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
    /// Serializes part cleanup and reservation changes, never body streaming.
    pub(crate) upload_budget: tokio::sync::Mutex<UploadBudget>,
    pub argon2: Arc<tokio::sync::Semaphore>,
    /// Thumbnail decodes in flight. Hardcoded to 2: a small thumbnail can
    /// need the full source bitmap in RAM, so this bounds peak decode memory.
    pub thumbs: tokio::sync::Semaphore,
    pub(crate) capture_times: tokio::sync::Semaphore,
    /// Read requests per IP (listing + photo), separate from uploads.
    pub read: Mutex<HashMap<IpAddr, WindowRing>>,
    /// Resumable-chunk requests per IP, separate from the upload bucket.
    pub chunks: Mutex<HashMap<IpAddr, WindowRing>>,
    /// Last full `.part-` sweep; healthz runs one at most once a minute.
    pub last_sweep: Mutex<Instant>,
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

/// chmod `path`; errors carry the path. Non-unix is a no-op.
fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| anyhow::anyhow!("chmod {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// `GALLY_MEDIA_SECRET` must be at least 32 bytes of key material.
fn env_media_secret(raw: &str) -> anyhow::Result<Vec<u8>> {
    if raw.len() < 32 {
        anyhow::bail!(
            "GALLY_MEDIA_SECRET must be at least 32 bytes (got {})",
            raw.len()
        );
    }
    Ok(raw.as_bytes().to_vec())
}

/// Warn once when the operator's root is readable by others or owned by a
/// different uid. Never refuse: bind mounts and legacy dirs must still run.
#[cfg(unix)]
fn warn_root_permissions(root: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(md) = std::fs::metadata(root) else {
        return;
    };
    let mode = md.permissions().mode() & 0o777;
    // SAFETY: `geteuid` has no preconditions and touches no memory.
    let foreign_owner = md.uid() != unsafe { libc::geteuid() };
    if mode & 0o077 != 0 || foreign_owner {
        tracing::warn!(
            path = %root.display(),
            mode = format!("{mode:o}"),
            uid = md.uid(),
            "GALLY_ROOT is accessible by other users or owned by another uid"
        );
    }
}

/// Owner-only on the SQLite file and its WAL sidecars; NotFound is fine.
fn restrict_db_files(db_path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut raw = db_path.as_os_str().to_os_string();
        raw.push(suffix);
        let path = PathBuf::from(raw);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => set_mode(&path, 0o600)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::anyhow!("stat {}: {e}", path.display())),
        }
    }
    Ok(())
}

/// HMAC key for media cookies: `GALLY_MEDIA_SECRET`, else a 32-byte secret
/// generated once into `<root>/.gally-media-secret` (0600) and reused.
fn media_secret(root: &Path) -> anyhow::Result<Vec<u8>> {
    if let Ok(s) = std::env::var("GALLY_MEDIA_SECRET") {
        return env_media_secret(&s);
    }
    let path = root.join(".gally-media-secret");
    match std::fs::read(&path) {
        Ok(raw) => {
            let text = String::from_utf8_lossy(&raw);
            if let Some(bytes) = from_hex(text.trim())
                && bytes.len() == 32
            {
                // A lax umask or a copied file may have left it 0644.
                set_mode(&path, 0o600)?;
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
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("chmod {}: {e}", path.display()))?;
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
        #[cfg(unix)]
        warn_root_permissions(root);
        // Photos hold every uploaded file: owner-only. The operator chose the
        // root's own mode, so leave that untouched.
        set_mode(&dir, 0o700)?;
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
            let missing_or_zero = match db_path.metadata() {
                Ok(md) => md.len() == 0,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(e.into()),
            };
            if missing_or_zero {
                db::require_no_originals(&scan_dir)?;
            }
            let mut conn = db::open(&db_path)?;
            // WAL sidecars exist once open ran; keep the whole DB owner-only.
            restrict_db_files(&db_path)?;
            if !missing_or_zero {
                let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                if version == 0
                    && !conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM galleries UNION ALL SELECT 1 FROM photos)",
                        [],
                        |r| r.get::<_, bool>(0),
                    )?
                {
                    db::require_no_originals(&scan_dir)?;
                }
            }
            db::reconcile_pending_deletes(&conn, &scan_dir)?;
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
            upload_budget: tokio::sync::Mutex::new(UploadBudget::default()),
            argon2: Arc::new(tokio::sync::Semaphore::new(limits.argon2_slots)),
            thumbs: tokio::sync::Semaphore::new(2),
            capture_times: tokio::sync::Semaphore::new(2),
            read: Mutex::new(HashMap::new()),
            chunks: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
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

#[derive(Default)]
pub(crate) struct UploadBudget {
    pub active: HashMap<String, UploadReservation>,
    pub idle: HashMap<String, u64>,
    // Fully received originals kept for recount after a failed DB update.
    pub unindexed: HashMap<String, u64>,
    // Never advertise a partial offset if rollback itself failed.
    pub damaged: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct UploadReservation {
    pub bytes: u64,
    pub unwritten: u64,
    pub flushed: u64,
}

impl UploadReservation {
    pub fn flush_bytes(&mut self, bytes: u64) {
        self.unwritten -= bytes;
        self.flushed += bytes;
    }
}

/// Monotonic whole seconds since the process started; shared by every ring so
/// their `last_sec` values are comparable for oldest-entry eviction.
fn window_sec() -> u64 {
    static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    EPOCH.elapsed().as_secs()
}

/// One-second-bucketed sliding window. 60 `u32` buckets (~240 bytes) replace a
/// vector that kept every timestamp for the window's duration.
/// shortcut: one-second granularity makes the effective window up to 1 s short
/// (a hit can expire early, never late), and the 10k-IP cap still evicts the
/// least recently used entry, not a true LRU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRing {
    buckets: [u32; RATE_WINDOW_SECS as usize],
    /// Process second of the newest bucket.
    last_sec: u64,
}

impl Default for WindowRing {
    fn default() -> Self {
        Self {
            buckets: [0; RATE_WINDOW_SECS as usize],
            last_sec: 0,
        }
    }
}

impl WindowRing {
    /// Drop buckets that left the ring. Denied hits never record, so expiring
    /// here cannot extend a window.
    fn expire(&mut self, now_sec: u64) {
        if now_sec <= self.last_sec {
            return;
        }
        if now_sec - self.last_sec >= RATE_WINDOW_SECS {
            self.buckets = [0; RATE_WINDOW_SECS as usize];
        } else {
            for sec in self.last_sec + 1..=now_sec {
                self.buckets[(sec % RATE_WINDOW_SECS) as usize] = 0;
            }
        }
        self.last_sec = now_sec;
    }

    /// Hits with `age < window_secs` (one-second granularity).
    fn recent(&self, window_secs: u64) -> u64 {
        debug_assert!(
            window_secs <= RATE_WINDOW_SECS,
            "ring only covers {RATE_WINDOW_SECS} seconds"
        );
        let window = window_secs.min(RATE_WINDOW_SECS);
        let mut total = 0;
        for age in 0..window {
            if age > self.last_sec {
                break;
            }
            total += u64::from(self.buckets[((self.last_sec - age) % RATE_WINDOW_SECS) as usize]);
        }
        total
    }

    fn record(&mut self, now_sec: u64) {
        let bucket = (now_sec % RATE_WINDOW_SECS) as usize;
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }

    /// Record one hit, or return how long until a slot opens.
    fn hit(&mut self, now_sec: u64, max: usize, window_secs: u64) -> Option<Duration> {
        debug_assert!(
            window_secs <= RATE_WINDOW_SECS,
            "ring only covers {RATE_WINDOW_SECS} seconds"
        );
        if max == 0 {
            return None;
        }
        self.expire(now_sec);
        let max = max as u64;
        let recent = self.recent(window_secs);
        if recent < max {
            self.record(now_sec);
            return None;
        }
        // Refuse without recording. The slot opens when the max-th newest hit
        // leaves the window.
        let window = window_secs.min(RATE_WINDOW_SECS);
        let mut seen = 0;
        for age in 0..window {
            if age > self.last_sec {
                break;
            }
            seen += u64::from(self.buckets[((self.last_sec - age) % RATE_WINDOW_SECS) as usize]);
            if seen >= max {
                return Some(Duration::from_secs(window.saturating_sub(age).max(1)));
            }
        }
        Some(Duration::from_secs(1))
    }
}

/// Sliding-window rate limit. A refused hit returns the time until a slot opens.
/// shortcut: 60 one-second buckets per IP, so memory is fixed at ~240 bytes;
/// the 10k-IP map evicts its oldest entry rather than adding a real LRU.
pub fn window_hit(
    map: &Mutex<HashMap<IpAddr, WindowRing>>,
    ip: &IpAddr,
    max: usize,
    window_secs: u64,
) -> Option<Duration> {
    window_hit_at(map, ip, max, window_secs, window_sec())
}

/// `window_hit` with an explicit clock, so tests can age a window without
/// sleeping 60 seconds.
fn window_hit_at(
    map: &Mutex<HashMap<IpAddr, WindowRing>>,
    ip: &IpAddr,
    max: usize,
    window_secs: u64,
    now_sec: u64,
) -> Option<Duration> {
    if max == 0 {
        return None;
    }
    let mut rings = map.lock().unwrap();
    // Bound memory: drop the oldest entry when full (dead IPs never return).
    if !rings.contains_key(ip)
        && rings.len() >= 10_000
        && let Some(oldest) = rings
            .iter()
            .min_by_key(|(_, ring)| ring.last_sec)
            .map(|(k, _)| *k)
    {
        rings.remove(&oldest);
    }
    rings.entry(*ip).or_default().hit(now_sec, max, window_secs)
}

/// Upload throttle (per IP).
pub fn rate_limited(state: &AppState, ip: &IpAddr) -> Option<Duration> {
    window_hit(&state.rate, ip, state.limits.rate_max, RATE_WINDOW_SECS)
}

/// Read throttle for listings and photo serving (per IP).
pub fn read_limited(state: &AppState, ip: &IpAddr) -> bool {
    window_hit(&state.read, ip, state.limits.read_max, RATE_WINDOW_SECS).is_some()
}

/// Resumable-chunk throttle (per IP); every chunk costs one hit.
pub fn chunk_limited(state: &AppState, ip: &IpAddr) -> Option<Duration> {
    window_hit(&state.chunks, ip, state.limits.chunk_max, RATE_WINDOW_SECS)
}

#[derive(Default)]
struct CredentialAttempts {
    failures: WindowRing,
    in_flight: u32,
}

impl CredentialAttempts {
    fn exhausted(&mut self, now_sec: u64) -> bool {
        self.failures.expire(now_sec);
        self.failures.recent(PASS_FAIL_WINDOW_SECS) + u64::from(self.in_flight)
            >= PASS_FAIL_MAX as u64
    }
}

fn credential_entry(
    attempts: &mut HashMap<IpAddr, CredentialAttempts>,
    ip: IpAddr,
) -> Option<&mut CredentialAttempts> {
    // Never evict an entry whose worker will settle an outstanding attempt.
    if attempts.len() >= 10_000 && !attempts.contains_key(&ip) {
        let oldest = attempts
            .iter()
            .filter(|(_, entry)| entry.in_flight == 0)
            .min_by_key(|(_, entry)| entry.failures.last_sec)
            .map(|(ip, _)| *ip)?;
        attempts.remove(&oldest);
    }
    Some(attempts.entry(ip).or_default())
}

/// Record one failed password/key attempt. Returns true once over the limit.
pub fn pass_fail_record(state: &AppState, ip: &IpAddr) -> bool {
    pass_fail_check(state, *ip, || false).is_none()
}

/// A cheap credential check and any resulting failure are one transaction.
pub(crate) fn pass_fail_check(
    state: &AppState,
    ip: IpAddr,
    check: impl FnOnce() -> bool,
) -> Option<bool> {
    let mut attempts = state.pass_fail.lock().unwrap();
    pass_fail_check_at(&mut attempts, ip, window_sec(), check)
}

fn pass_fail_check_at(
    attempts: &mut HashMap<IpAddr, CredentialAttempts>,
    ip: IpAddr,
    now_sec: u64,
    check: impl FnOnce() -> bool,
) -> Option<bool> {
    if attempts.get_mut(&ip).is_some_and(|e| e.exhausted(now_sec)) {
        return None;
    }
    let ok = check();
    if !ok {
        let failures = &mut credential_entry(attempts, ip)?.failures;
        failures.expire(now_sec);
        failures.record(now_sec);
    }
    Some(ok)
}

#[derive(Debug)]
pub(crate) enum ViewerAdmission {
    RateLimited,
    Busy,
}

/// The blocking job owns both the hash slot and the per-IP reservation.
pub(crate) struct ViewerAttempt {
    state: Arc<AppState>,
    ip: IpAddr,
    failed: bool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl ViewerAttempt {
    pub(crate) fn try_begin(state: &Arc<AppState>, ip: IpAddr) -> Result<Self, ViewerAdmission> {
        let mut attempts = state.pass_fail.lock().unwrap();
        if attempts
            .get_mut(&ip)
            .is_some_and(|e| e.exhausted(window_sec()))
        {
            return Err(ViewerAdmission::RateLimited);
        }
        let permit = Arc::clone(&state.argon2)
            .try_acquire_owned()
            .map_err(|_| ViewerAdmission::Busy)?;
        credential_entry(&mut attempts, ip)
            .ok_or(ViewerAdmission::RateLimited)?
            .in_flight += 1;
        Ok(Self {
            state: Arc::clone(state),
            ip,
            failed: false,
            _permit: permit,
        })
    }

    pub(crate) fn finish(mut self, verified_key: Option<Vec<u8>>) {
        if let Some(key) = verified_key {
            let mut cache = self.state.pass_ok.lock().unwrap();
            if cache.len() >= 10_000 {
                cache.retain(|_, at| at.elapsed() < PASS_CACHE_TTL);
            }
            cache.insert(key, Instant::now());
        } else {
            self.failed = true;
        }
    }
}

impl Drop for ViewerAttempt {
    fn drop(&mut self) {
        let mut attempts = self.state.pass_fail.lock().unwrap();
        let std::collections::hash_map::Entry::Occupied(mut attempt) = attempts.entry(self.ip)
        else {
            unreachable!("an in-flight attempt cannot be evicted");
        };
        let entry = attempt.get_mut();
        entry.in_flight -= 1;
        let now_sec = window_sec();
        entry.failures.expire(now_sec);
        if self.failed {
            entry.failures.record(now_sec);
        } else if entry.in_flight == 0 && entry.failures.recent(PASS_FAIL_WINDOW_SECS) == 0 {
            attempt.remove();
        }
    }
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

    #[test]
    fn denied_credentials_do_not_extend_the_window() {
        let mut attempts = HashMap::new();
        let ip = IpAddr::from([127, 0, 0, 1]);
        for _ in 0..PASS_FAIL_MAX {
            assert_eq!(
                pass_fail_check_at(&mut attempts, ip, 0, || false),
                Some(false)
            );
        }
        for now_sec in 1..PASS_FAIL_WINDOW_SECS {
            assert!(pass_fail_check_at(&mut attempts, ip, now_sec, || panic!("checked")).is_none());
        }
        assert_eq!(
            pass_fail_check_at(&mut attempts, ip, PASS_FAIL_WINDOW_SECS, || false),
            Some(false)
        );
    }

    #[tokio::test]
    async fn in_flight_viewers_share_the_credential_budget() {
        let root = tempfile::tempdir().unwrap();
        let state = AppState::new_with(
            root.path(),
            Limits {
                argon2_slots: PASS_FAIL_MAX + 1,
                ..Limits::defaults()
            },
        )
        .await
        .unwrap();
        let ip = IpAddr::from([127, 0, 0, 1]);
        let another_ip = IpAddr::from([127, 0, 0, 2]);
        for _ in 1..PASS_FAIL_MAX {
            assert!(!pass_fail_record(&state, &ip));
        }
        let viewer = ViewerAttempt::try_begin(&state, ip).unwrap();
        assert!(matches!(
            ViewerAttempt::try_begin(&state, ip),
            Err(ViewerAdmission::RateLimited)
        ));
        assert!(pass_fail_check(&state, ip, || panic!("checked")).is_none());
        assert!(ViewerAttempt::try_begin(&state, another_ip).is_ok());
        viewer.finish(Some(vec![1]));
        assert_eq!(pass_fail_check(&state, ip, || true), Some(true));
        ViewerAttempt::try_begin(&state, ip).unwrap().finish(None);
        assert!(pass_fail_record(&state, &ip));
    }

    #[test]
    fn window_ring_expires_after_sixty_seconds_and_refuses_when_full() {
        let mut ring = WindowRing::default();
        assert!(ring.hit(0, 1, RATE_WINDOW_SECS).is_none());
        // Over the cap: refused, and the refused hit is not recorded.
        assert!(ring.hit(1, 1, RATE_WINDOW_SECS).is_some());
        assert_eq!(ring.recent(RATE_WINDOW_SECS), 1);
        // Exactly 60 seconds after the recorded hit the window is empty again.
        assert!(ring.hit(60, 1, RATE_WINDOW_SECS).is_none());
        assert!(ring.hit(60, 1, RATE_WINDOW_SECS).is_some());
    }

    #[test]
    fn window_map_cap_evicts_one_entry() {
        let map = Mutex::new(HashMap::new());
        for i in 0..10_000u32 {
            let ip = IpAddr::from(i.to_be_bytes());
            assert!(window_hit_at(&map, &ip, 1, RATE_WINDOW_SECS, 0).is_none());
        }
        assert_eq!(map.lock().unwrap().len(), 10_000);
        let fresh = IpAddr::from([255, 255, 255, 255]);
        assert!(window_hit_at(&map, &fresh, 1, RATE_WINDOW_SECS, 1).is_none());
        let rings = map.lock().unwrap();
        assert_eq!(rings.len(), 10_000, "one entry evicted, cap held");
        assert!(rings.contains_key(&fresh));
    }

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
    fn env_media_secret_requires_32_bytes() {
        let err = env_media_secret(&"a".repeat(31)).expect_err("31 bytes must fail");
        assert!(err.to_string().contains("at least 32 bytes"), "{err}");
        let ok = env_media_secret(&"b".repeat(32)).expect("32 bytes is valid");
        assert_eq!(ok, vec![b'b'; 32]);
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
