//! Maintenance: drop galleries that never got content, so dead links don't pile up.

use std::path::Path;
use std::sync::Arc;

use crate::db::GalleryDb;
use crate::state::AppState;

/// Grace period: a gallery minted now may still receive its first upload.
pub const EMPTY_TTL_SECS: i64 = 5 * 60;

/// Delete galleries older than the grace period that have no media files and
/// no canvas content. Runs at startup and on a timer (see `main`).
pub async fn sweep(state: &Arc<AppState>) {
    let candidates = {
        let db = state.db.lock().unwrap();
        db.empty_galleries(EMPTY_TTL_SECS)
    };
    for slug in candidates {
        if has_media(&state.dir, &format!("{slug}_")).await {
            continue;
        }
        let db = state.db.lock().unwrap();
        if db.delete_gallery_rows(&slug) {
            tracing::info!(slug = %slug, "purged empty gallery");
        }
    }
}

/// Any file for this gallery? Unknown (unreadable dir, scan error) counts as
/// "has media": never delete a gallery we cannot fully inspect.
async fn has_media(dir: &Path, prefix: &str) -> bool {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return true;
    };
    loop {
        match rd.next_entry().await {
            Ok(Some(ent)) => {
                if ent.file_name().to_string_lossy().starts_with(prefix) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}
