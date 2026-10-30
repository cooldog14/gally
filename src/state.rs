use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::{self, GalleryDb};

pub const RATE_MAX: usize = 60;
pub const RATE_WINDOW_SECS: u64 = 60;

/// How long a verified (hash, password) pair stays accepted.
pub const PASS_CACHE_TTL: Duration = Duration::from_secs(300);
/// Failed password/key attempts allowed per IP per window.
pub const PASS_FAIL_MAX: usize = 10;
pub const PASS_FAIL_WINDOW_SECS: u64 = 60;

/// Upload cap per IP per window. `GALLY_RATE_MAX` overrides (0 = unlimited).
pub fn rate_max() -> usize {
    std::env::var("GALLY_RATE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RATE_MAX)
}

// (Listing cache removed: plain directory scans never go stale.)

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
    pub statics: HashMap<&'static str, (&'static [u8], &'static str)>,
    /// Invite code gating NEW gallery creation (`GALLY_INVITE_CODE` at
    /// startup; `None` = open creation). Contributing to an existing
    /// gallery never needs it.
    pub invite: Mutex<Option<String>>,
}

impl AppState {
    pub async fn new(root: &Path) -> anyhow::Result<Arc<AppState>> {
        let dir = root.join("photos");
        tokio::fs::create_dir_all(&dir).await?;
        // An upload that died mid-stream leaves a `.part-` temp behind; they
        // are never listed, so drop them here rather than let them pile up.
        if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                if ent.file_name().to_string_lossy().starts_with(".part-") {
                    let _ = tokio::fs::remove_file(ent.path()).await;
                }
            }
        }
        // db::open is synchronous but fast (local SQLite open + DDL); run
        // it blocking so we never stall the runtime.
        let db_path = root.join("galleries.db");
        let conn = tokio::task::spawn_blocking(move || db::open(&db_path))
            .await??;
        // Touch GalleryDb trait so method resolution is checked here too.
        let _ = conn.gallery_exists("");
        let statics: HashMap<&'static str, (&'static [u8], &'static str)> =
            HashMap::from([
                ("/", (&include_bytes!("../index.html")[..], "text/html")),
                (
                    "/index.html",
                    (&include_bytes!("../index.html")[..], "text/html"),
                ),
                (
                    "/app.js",
                    (
                        &include_bytes!("../app.js")[..],
                        "application/javascript",
                    ),
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
        Ok(Arc::new(AppState {
            canon_dir: dir.canonicalize().unwrap_or_else(|_| dir.clone()),
            dir,
            db: Mutex::new(conn),
            admin_tokens: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
            pass_ok: Mutex::new(HashMap::new()),
            pass_fail: Mutex::new(HashMap::new()),
            statics,
            invite: Mutex::new(std::env::var("GALLY_INVITE_CODE").ok().filter(|s| !s.trim().is_empty())),
        }))
    }
}

/// Fixed-window rate limit: at most `rate_max()` hits per window (0 = off).
/// Returns true when the caller is over the limit (hit NOT recorded).
/// shortcut: hard cap at 10k IPs, evicts oldest — switch to LRU when NAT-heavy.
pub fn rate_limited(state: &AppState, ip: &IpAddr) -> bool {
    let max = rate_max();
    if max == 0 {
        return false;
    }
    let now = Instant::now();
    let mut rate = state.rate.lock().unwrap();
    // Bound memory: drop the oldest entry when full (dead IPs never return).
    if !rate.contains_key(ip) && rate.len() >= 10_000 {
        if let Some(oldest) = rate.iter().min_by_key(|(_, v)| v.first().copied()).map(|(k, _)| *k) {
            rate.remove(&oldest);
        }
    }
    let hits = rate.entry(*ip).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs() < RATE_WINDOW_SECS);
    if hits.len() >= max {
        return true;
    }
    hits.push(now);
    false
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
    if !fail.contains_key(ip) && fail.len() >= 10_000 {
        if let Some(oldest) = fail.iter().min_by_key(|(_, v)| v.first().copied()).map(|(k, _)| *k) {
            fail.remove(&oldest);
        }
    }
    let hits = fail.entry(*ip).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs() < PASS_FAIL_WINDOW_SECS);
    hits.push(now);
    hits.len() > PASS_FAIL_MAX
}

/// Admin tokens live 24h from issue; expired ones are dropped on sight.
pub const ADMIN_TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);

/// True when the request carries a live `admin=` cookie value.
pub fn is_admin(headers: &axum::http::HeaderMap, state: &AppState) -> bool {
    let raw = match headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        Some(c) => c,
        None => return false,
    };
    let tok = raw
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("admin="));
    match tok {
        Some(t) => {
            let mut tokens = state.admin_tokens.lock().unwrap();
            match tokens.get(t) {
                Some(issued) if issued.elapsed() < ADMIN_TOKEN_TTL => true,
                // Expired (or just removed): drop it so the map never grows.
                _ => {
                    tokens.remove(t);
                    false
                }
            }
        }
        None => false,
    }
}
