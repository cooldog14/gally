use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;

pub mod db;
pub mod routes;
pub mod state;
pub mod validate;

pub use state::AppState;

/// Log every request with its latency.
async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let t0 = Instant::now();
    let resp = next.run(req).await;
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(method = %method, path = %path, status = %resp.status(), latency_ms = format!("{ms:.2}"));
    resp
}
pub fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, get, post, put};
    use routes::*;
    Router::new()
        .route("/api/galleries/{slug}/photos", get(gallery_photos))
        .route("/api/admin/galleries", get(admin_galleries))
        .route("/api/admin/banned", get(admin_banned))
        .route("/api/photos/{*name}", put(put_photo))
        .route("/api/photos/{*name}", delete(delete_photo))
        .route("/photos/{*name}", get(get_photo))
        .route("/api/galleries/resolve", post(resolve_gallery))
        .route("/api/admin/unlock", post(admin_unlock))
        .route("/api/admin/galleries/{slug}/delete", post(delete_gallery))
        .route("/api/admin/galleries/{slug}/delete", delete(delete_gallery))
        .route("/api/galleries/{slug}/lock", post(lock_gallery))
        .route("/api/admin/unban", post(admin_unban))
        .route("/api/register", post(gone_compat))
        .route("/api/login", post(gone_compat))
        .route("/api/logout", post(gone_compat))
        .route("/gally/admin/", get(admin_page))
        .route("/gally/admin", get(admin_page_redirect))
        .route("/app.js", get(static_path))
        .route("/vendor-heic2any.min.js", get(static_path))
        .route("/styles.css", get(static_path))
        .route("/index.html", get(static_path))
        .route("/fonts/Figtree-400.woff2", get(static_path))
        .route("/fonts/Figtree-500.woff2", get(static_path))
        .route("/fonts/Figtree-600.woff2", get(static_path))
        .route("/", get(static_path))
        .route("/{slug}", get(spa_or_404))
        .fallback(fallback_404)
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}
