use std::sync::Arc;

use axum::Router;

pub mod db;
pub mod routes;
pub mod state;
pub mod validate;

pub use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, get, post, put};
    use routes::*;
    Router::new()
        .route("/api/galleries/{slug}/photos", get(gallery_photos))
        .route("/api/admin/galleries", get(admin_galleries))
        .route("/api/admin/banned", get(admin_banned))
        .route("/api/me", get(api_me))
        .route("/api/photos/{*name}", put(put_photo))
        .route("/api/photos/{*name}", delete(delete_photo))
        .route("/photos/{*name}", get(get_photo))
        .route("/api/galleries/resolve", post(resolve_gallery))
        .route("/api/admin/unlock", post(admin_unlock))
        .route("/api/admin/galleries/{slug}/delete", post(delete_gallery))
        .route("/api/admin/galleries/{slug}/delete", delete(delete_gallery))
        .route("/api/admin/unban", post(admin_unban))
        .route("/api/register", post(gone_compat))
        .route("/api/login", post(gone_compat))
        .route("/api/logout", post(gone_compat))
        .route("/app.js", get(static_path))
        .route("/styles.css", get(static_path))
        .route("/index.html", get(static_path))
        .route("/fonts/Figtree-400.woff2", get(static_path))
        .route("/fonts/Figtree-500.woff2", get(static_path))
        .route("/fonts/Figtree-600.woff2", get(static_path))
        .route("/", get(static_path))
        .route("/{slug}", get(spa_or_404))
        .fallback(fallback_404)
        .with_state(state)
}
