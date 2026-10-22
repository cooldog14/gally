use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::db::{self, GalleryDb};

pub const RATE_MAX: usize = 60;
pub const RATE_WINDOW_SECS: u64 = 60;

pub struct AppState {
    pub dir: PathBuf,
    pub db: Mutex<rusqlite::Connection>,
    pub admin_tokens: Mutex<HashSet<String>>,
    pub rate: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    pub statics: HashMap<&'static str, (&'static [u8], &'static str)>,
}

impl AppState {
    pub async fn new(root: &Path) -> anyhow::Result<Arc<AppState>> {
        let dir = root.join("photos");
        tokio::fs::create_dir_all(&dir).await?;
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
            dir,
            db: Mutex::new(conn),
            admin_tokens: Mutex::new(HashSet::new()),
            rate: Mutex::new(HashMap::new()),
            statics,
        }))
    }
}

/// Fixed-window rate limit: at most RATE_MAX hits per RATE_WINDOW_SECS.
/// Returns true when the caller is over the limit (hit NOT recorded).
pub fn rate_limited(state: &AppState, ip: &IpAddr) -> bool {
    let now = Instant::now();
    let mut rate = state.rate.lock().unwrap();
    let hits = rate.entry(*ip).or_default();
    hits.retain(|t| now.duration_since(*t).as_secs() < RATE_WINDOW_SECS);
    if hits.len() >= RATE_MAX {
        return true;
    }
    hits.push(now);
    false
}

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
        Some(t) => state.admin_tokens.lock().unwrap().contains(t),
        None => false,
    }
}
