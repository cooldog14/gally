use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;

pub mod backup;
pub mod db;
mod exif_time;
pub mod routes;
pub mod state;
pub mod validate;

pub use state::AppState;

/// Static/immutable paths: logged at `debug` so real requests stay visible.
fn is_noisy(path: &str) -> bool {
    matches!(
        path,
        "/" | "/index.html"
            | "/app.js"
            | "/vendor-heic2any.min.js"
            | "/styles.css"
            | "/healthz"
            | "/metrics"
    ) || path.starts_with("/fonts/")
        || path.starts_with("/photos/")
}

/// Log every request with its client IP and latency, and stamp the security
/// headers that do not depend on the body.
async fn log_requests(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    state.requests_total.fetch_add(1, Ordering::Relaxed);
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    // Same resolution as the handlers: GALLY_TRUST_PROXY lets a trusted
    // reverse proxy's X-Forwarded-For stand in for the peer address here too.
    let info = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|c| ConnectInfo(c.0));
    let ip = routes::client_ip(req.headers(), info).to_string();
    let t0 = Instant::now();
    let mut resp = next.run(req).await;
    if resp.status().is_server_error() {
        state.responses_5xx_total.fetch_add(1, Ordering::Relaxed);
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    // Media and statics dominate the log; uploads/navigation stay at info.
    if method == axum::http::Method::GET && is_noisy(&path) {
        tracing::debug!(ip = %ip, method = %method, path = %path, status = %resp.status(), latency_ms = format!("{ms:.2}"));
    } else {
        tracing::info!(ip = %ip, method = %method, path = %path, status = %resp.status(), latency_ms = format!("{ms:.2}"));
    }
    let headers = resp.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    // Handlers may set a stricter policy (SVG: `sandbox`); never downgrade it.
    headers
        .entry("content-security-policy")
        .or_insert("frame-ancestors 'none'".parse().unwrap());
    resp
}

pub fn router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, get, post, put};
    use routes::*;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/galleries/{slug}/photos", get(gallery_photos))
        .route("/api/galleries/{slug}", delete(delete_own_gallery))
        .route("/api/galleries/{slug}/visibility", post(set_visibility))
        .route("/api/admin/galleries", get(admin_galleries))
        .route("/api/admin/sweep", post(admin_sweep))
        .route("/api/admin/logout", post(admin_logout))
        .route("/api/photos/{*name}", put(put_photo))
        .route("/api/photos/{*name}", delete(delete_photo))
        .route("/photos/thumb/{*name}", get(get_thumb))
        .route("/photos/{*name}", get(get_photo))
        .route("/api/galleries/resolve", post(resolve_gallery))
        .route("/api/admin/unlock", post(admin_unlock))
        .route("/api/admin/galleries/{slug}/remove", post(remove_gallery))
        .route("/api/admin/photos/{*name}", delete(admin_delete_photo))
        .route("/api/galleries/{slug}/lock", post(lock_gallery))
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
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .with_state(state)
}
