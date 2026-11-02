use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use gally_uring::db::GalleryDb;
use gally_uring::{router, state::Limits, validate::is_slug, AppState};
use tower::ServiceExt;

const KEY_A: &str = "test-key-alpha";
const KEY_B: &str = "test-key-other";

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    test_state_with(Limits::defaults()).await
}

async fn test_state_with(limits: Limits) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = AppState::new_with(tmp.path(), limits).await.expect("AppState::new_with");
    (state, tmp)
}

/// Publish a gallery (new galleries start private).
async fn publish(state: &Arc<AppState>, slug: &str, public: bool) -> StatusCode {
    let r = post_json(
        &format!("/api/galleries/{slug}/visibility"),
        &[key_header(KEY_A)],
        &format!(r#"{{"public":{public}}}"#),
    );
    router(state.clone()).oneshot(r).await.unwrap().status()
}

fn sock() -> SocketAddr {
    "127.0.0.1:4242".parse().unwrap()
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: Body) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut r = b.body(body).unwrap();
    r.extensions_mut().insert(ConnectInfo(sock()));
    r
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    request("GET", uri, headers, Body::empty())
}

fn post_json(uri: &str, headers: &[(&str, &str)], json: &str) -> Request<Body> {
    let mut h = vec![("content-type", "application/json")];
    h.extend_from_slice(headers);
    request("POST", uri, &h, Body::from(json.to_string()))
}

async fn body_json(resp: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_bytes(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

/// Insert a live admin token directly and return its `Cookie` header value.
fn admin_cookie(state: &Arc<AppState>) -> String {
    let tok = "test-admin-token".to_string();
    state.admin_tokens.lock().unwrap().insert(tok.clone(), std::time::Instant::now());
    format!("admin={tok}")
}

/// `ADMIN_PASS` is process-global: every test that sets/removes it must hold
/// this lock for its whole body, or parallel tests race each other.
static ADMIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock_admin_env() -> std::sync::MutexGuard<'static, ()> {
    ADMIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn key_header(key: &str) -> (&str, &str) {
    ("x-gallery-key", key)
}

/// Upload `bytes` as `name`; returns (status, parsed json body).
/// `slug`: gallery-page upload (scoped). None = home upload (fresh gallery).
async fn put_photo(
    state: &Arc<AppState>,
    name: &str,
    key: Option<&str>,
    slug: Option<&str>,
    ctype: &str,
    bytes: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    let mut h = vec![("content-type", ctype)];
    if let Some(k) = key {
        h.push(key_header(k));
    }
    let slug_owned;
    if let Some(s) = slug {
        slug_owned = s.to_string();
        h.push(("x-gallery-slug", slug_owned.as_str()));
    }
    let uri = format!("/api/photos/{name}");
    let r = request("PUT", &uri, &h, Body::from(bytes));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

fn jpeg() -> Vec<u8> {
    vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46]
}

async fn resolve(state: &Arc<AppState>, key: &str) -> (StatusCode, serde_json::Value) {
    let r = post_json(
        "/api/galleries/resolve",
        &[],
        &format!(r#"{{"key":{key:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

// --- statics ---

#[tokio::test]
async fn static_root_serves_html() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(!body_bytes(resp).await.is_empty());
}

#[tokio::test]
async fn static_app_js_serves_javascript() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/app.js", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/javascript")
    );
    assert!(!body_bytes(resp).await.is_empty());
}

#[tokio::test]
async fn unknown_path_is_404_json() {
    let (state, _tmp) = test_state().await;
    let resp = router(state)
        .oneshot(get("/this/path/does/not/exist", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert!(v.get("error").is_some(), "{v}");
}

// --- gallery resolve + listing ---

#[tokio::test]
async fn resolve_returns_slug_shape() {
    let (state, _tmp) = test_state().await;
    let (status, v) = resolve(&state, KEY_A).await;
    assert_eq!(status, StatusCode::OK);
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap();
    assert!(is_slug(slug), "{slug}");
}

#[tokio::test]
async fn same_key_mints_distinct_galleries() {
    let (state, _tmp) = test_state().await;
    // Same password, two home uploads: two galleries, not one.
    let (_, v1) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let (_, v2) = put_photo(&state, "b.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let s1 = v1.get("slug").and_then(|s| s.as_str()).unwrap();
    let s2 = v2.get("slug").and_then(|s| s.as_str()).unwrap();
    assert_ne!(s1, s2);
    // Scoped auth: the key owns both slugs explicitly.
    for s in [s1, s2] {
        let resp = router(state.clone())
            .oneshot(get(&format!("/api/galleries/{s}/photos"), &[key_header(KEY_A)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Explicit slug header lands in that gallery (gallery-page upload).
    let r = request(
        "PUT",
        "/api/photos/stray.jpg",
        &[("content-type", "image/jpeg"), key_header(KEY_A), ("x-gallery-slug", s1)],
        Body::from(jpeg()),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Unknown-slug header: rejected, never minted.
    let r = request(
        "PUT",
        "/api/photos/stray2.jpg",
        &[("content-type", "image/jpeg"), key_header(KEY_A), ("x-gallery-slug", "quiet-river-calm-pond-1a2b")],
        Body::from(jpeg()),
    );
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- invite gate ---

fn invite_header(code: &str) -> (&str, &str) {
    ("x-invite-code", code)
}

#[tokio::test]
async fn open_creation_without_invite_configured() {
    // No GALLY_INVITE_CODE in test env: new galleries mint freely.
    let (state, _tmp) = test_state().await;
    assert!(state.invite.lock().unwrap().is_none());
    let (status, v) = put_photo(&state, "open.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(v.get("slug").and_then(|s| s.as_str()).is_some());
}

#[tokio::test]
async fn new_gallery_needs_invite_when_configured() {
    let (state, _tmp) = test_state().await;
    state.invite.lock().unwrap().replace("amber-fox-glade-ridge".to_string());

    // No invite: creation refused, nothing stored.
    let (status, v) =
        put_photo(&state, "gated.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        v.get("error").and_then(|e| e.as_str()),
        Some("invite required")
    );

    // Wrong invite: still refused.
    let h = vec![("content-type", "image/jpeg"), key_header(KEY_A), invite_header("nope")];
    let uri = "/api/photos/gated.jpg".to_string();
    let r = request("PUT", &uri, &h, Body::from(jpeg()));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Right invite: gallery mints.
    let h = vec![
        ("content-type", "image/jpeg"),
        key_header(KEY_A),
        invite_header("amber-fox-glade-ridge"),
    ];
    let r = request("PUT", &uri, &h, Body::from(jpeg()));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first_slug = body_json(resp).await.get("slug").and_then(|s| s.as_str()).unwrap().to_string();

    // Existing gallery: contribution targets the slug, needs no invite.
    let (status, v) =
        put_photo(&state, "second.jpg", Some(KEY_A), Some(&first_slug), "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v.get("slug").and_then(|s| s.as_str()), Some(first_slug.as_str()));
}

#[tokio::test]
async fn resolve_needs_invite_for_new_gallery() {
    let (state, _tmp) = test_state().await;
    state.invite.lock().unwrap().replace("amber-fox-glade-ridge".to_string());

    let (status, _) = resolve(&state, KEY_B).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let r = post_json(
        "/api/galleries/resolve",
        &[invite_header("amber-fox-glade-ridge")],
        &format!(r#"{{"key":{KEY_B:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn resolve_banned_key_is_403() {
    let (state, _tmp) = test_state().await;
    let (_, v) = resolve(&state, KEY_A).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();

    let cookie = admin_cookie(&state);
    let uri = format!("/api/admin/galleries/{slug}/delete");
    let r = request("DELETE", &uri, &[("cookie", cookie.as_str())], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (status, v) = resolve(&state, KEY_A).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v.get("error").and_then(|e| e.as_str()), Some("gallery banned"));
}

#[tokio::test]
async fn banned_key_reports_banned_despite_invite_gate() {
    let (state, _tmp) = test_state().await;
    let (_, v) = resolve(&state, KEY_A).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    let cookie = admin_cookie(&state);
    let uri = format!("/api/admin/galleries/{slug}/delete");
    let r = request("DELETE", &uri, &[("cookie", cookie.as_str())], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    state.invite.lock().unwrap().replace("amber-fox-glade-ridge".to_string());
    // Ban wins over "invite required": same key stays banned with or
    // without a valid invite, on both resolve and upload paths.
    let (status, v) = resolve(&state, KEY_A).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v.get("error").and_then(|e| e.as_str()), Some("gallery banned"));
    let r = post_json(
        "/api/galleries/resolve",
        &[invite_header("amber-fox-glade-ridge")],
        &format!(r#"{{"key":{KEY_A:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await.get("error").and_then(|e| e.as_str()),
        Some("gallery banned")
    );
    let (status, v) =
        put_photo(&state, "banned.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v.get("error").and_then(|e| e.as_str()), Some("gallery banned"));
}

#[tokio::test]
async fn gallery_listing_empty_then_contains_upload() {
    let (state, _tmp) = test_state().await;
    let (_, v) = resolve(&state, KEY_A).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();

    let uri = format!("/api/galleries/{slug}/photos");
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v.get("slug").and_then(|s| s.as_str()), Some(slug.as_str()));
    assert_eq!(v.get("photos").unwrap().as_array().unwrap().len(), 0);

    // Gallery-page contribution targets the resolved slug.
    let (status, v) = put_photo(&state, "pic.jpg", Some(KEY_A), Some(&slug), "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::CREATED);
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let photos = v.get("photos").unwrap().as_array().unwrap();
    assert_eq!(photos.len(), 1);
    assert_eq!(
        photos[0].get("name").and_then(|n| n.as_str()),
        Some(name.as_str())
    );
}

// --- PUT validation ---

#[tokio::test]
async fn put_roundtrip_bytes_identical() {
    let (state, _tmp) = test_state().await;
    let bytes = jpeg();
    let (status, v) = put_photo(&state, "shot.jpg", Some(KEY_A), None, "image/jpeg", bytes.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/photos/{name}");
    let resp = router(state)
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(body_bytes(resp).await, bytes);
}

#[tokio::test]
async fn put_missing_key_is_401() {
    let (state, _tmp) = test_state().await;
    let (status, _) = put_photo(&state, "shot.jpg", None, None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_any_ctype_accepted() {
    let (state, _tmp) = test_state().await;
    let (status, v) = put_photo(&state, "shot.jpg", Some(KEY_A), None, "text/plain", b"hi".to_vec()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(v.get("name").and_then(|n| n.as_str()).unwrap().ends_with("shot.jpg"));
}

#[tokio::test]
async fn put_any_extension_accepted() {
    let (state, _tmp) = test_state().await;
    let (status, v) = put_photo(&state, "notes.txt", Some(KEY_A), None, "text/plain", b"hi".to_vec()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(v.get("name").and_then(|n| n.as_str()).unwrap().ends_with("notes.txt"));
}

#[tokio::test]
async fn put_oversize_body_is_413() {
    // Limits are per-state now: no process-wide env to race with.
    let (state, _tmp) =
        test_state_with(Limits { max_file: 16, ..Limits::defaults() }).await;
    let big = vec![0u8; 17];
    let (status, _) = put_photo(&state, "big.jpg", Some(KEY_A), None, "image/jpeg", big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// --- photo serve / delete ---

#[tokio::test]
async fn get_photo_missing_is_404() {
    let (state, _tmp) = test_state().await;
    let resp = router(state)
        .oneshot(get("/photos/nope.jpg", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_by_owner_succeeds() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "gone.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name}");
    let r = request("DELETE", &uri, &[key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let uri = format!("/photos/{name}");
    let resp = router(state).oneshot(get(&uri, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_last_photo_keeps_gallery() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "only.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name}");
    let r = request("DELETE", &uri, &[key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await.get("gallery_gone"), Some(&serde_json::Value::Bool(false)));

    // Gallery row stays: owner's empty listing is 200 with no photos.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router(state).oneshot(get(&format!("/{slug}"), &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn lock_unlock_cycle() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let uri = format!("/api/galleries/{slug}/lock");
    // Lock with the owner key.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"password":"s3cret-pass"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Listing without password: 401 + locked flag.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await.get("locked"), Some(&serde_json::Value::Bool(true)));
    // Wrong password still 401.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos?pass=nope"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // Right password via query: 200.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos?pass=s3cret-pass"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Owner key bypasses without password.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Unlock: empty password clears the lock.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"password":""}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router(state)
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn lock_requires_owner_key() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    let uri = format!("/api/galleries/{slug}/lock");
    let r = post_json(&uri, &[key_header(KEY_B)], r#"{"password":"x"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn delete_keeps_gallery_when_photos_remain() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "one.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    let (_, v2) = put_photo(&state, "two.jpg", Some(KEY_A), Some(&slug), "image/jpeg", jpeg()).await;
    let name2 = v2.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name2}");
    let r = request("DELETE", &uri, &[key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await.get("gallery_gone"), Some(&serde_json::Value::Bool(false)));

    let resp = router(state)
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
#[tokio::test]
async fn delete_by_stranger_is_403() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name}");
    let r = request("DELETE", &uri, &[key_header(KEY_B)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Still there (private gallery: the owner key is the only reader).
    let uri = format!("/photos/{name}");
    let resp = router(state)
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn delete_by_admin_succeeds() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mod.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let cookie = admin_cookie(&state);
    let uri = format!("/api/photos/{name}");
    let r = request(
        "DELETE",
        &uri,
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- admin ---

#[tokio::test]
async fn admin_unlock_without_pass_is_404() {
    let _env = lock_admin_env();
    // Only tests holding ADMIN_ENV_LOCK touch the process env, so
    // remove/restore here is race-free within this binary.
    let saved = std::env::var("ADMIN_PASS").ok();
    // SAFETY: only this test touches the process env in this binary.
    unsafe { std::env::remove_var("ADMIN_PASS"); }
    let (state, _tmp) = test_state().await;
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"x"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    if let Some(v) = saved {
        // SAFETY: same as above.
        unsafe { std::env::set_var("ADMIN_PASS", v); }
    }
}
#[tokio::test]
async fn admin_endpoints_reject_unauth() {
    let (state, _tmp) = test_state().await;
    for uri in ["/api/admin/galleries", "/api/admin/banned"] {
        let resp = router(state.clone())
            .oneshot(get(uri, &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn admin_endpoints_accept_token() {
    let (state, _tmp) = test_state().await;
    let cookie = admin_cookie(&state);
    for uri in ["/api/admin/galleries", "/api/admin/banned"] {
        let resp = router(state.clone())
            .oneshot(get(uri, &[("cookie", cookie.as_str())]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn admin_page_serves_html() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/gally/admin/", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ctype.starts_with("text/html"), "{ctype}");
    let body = body_bytes(resp).await;
    assert!(body.windows(b"/api/admin/unlock".len()).any(|w| w == b"/api/admin/unlock"));
}

#[tokio::test]
async fn admin_page_redirect_no_slash() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/gally/admin", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(resp.headers().get("location").unwrap(), "/gally/admin/");
}

#[tokio::test]
async fn gally_prefix_is_not_gallery() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/gally", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gallery_slug_serves_spa() {
    let (state, _tmp) = test_state().await;
    let (_status, body) = put_photo(&state, "a.png", Some(KEY_A), None, "image/png", jpeg()).await;
    let slug = body["slug"].as_str().unwrap().to_string();
    let resp = router(state).oneshot(get(&format!("/{slug}"), &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ctype.starts_with("text/html"), "{ctype}");
}

#[tokio::test]
async fn unknown_gallery_slug_redirects_home() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/quiet-river-calm-pond-1a2b", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers().get("location").unwrap(), "/");
}

// --- compat stubs ---

#[tokio::test]
async fn compat_register_is_gone() {
    let (state, _tmp) = test_state().await;
    let r = post_json("/api/register", &[], "{}");
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn compat_login_logout_are_gone() {
    let (state, _tmp) = test_state().await;
    for uri in ["/api/login", "/api/logout"] {
        let r = post_json(uri, &[], "{}");
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::GONE);
        assert_eq!(body_json(resp).await.get("error"), Some(&serde_json::Value::String("gone".into())));
    }
}

// --- stream C findings ---

#[tokio::test]
async fn fix_c_malformed_lock_body_is_400_and_keeps_state() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let uri = format!("/api/galleries/{slug}/lock");
    // Malformed JSON must not clear (or set) the lock: 400, gallery stays open.
    let r = post_json(&uri, &[key_header(KEY_A)], "not json{{{");
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Empty body is likewise a 400, not a silent clear.
    let r = request("POST", &uri, &[("content-type", "application/json"), key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn fix_c_wrong_admin_password_is_403() {
    let _env = lock_admin_env();
    let (state, _tmp) = test_state().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe { std::env::set_var("ADMIN_PASS", "correct-horse"); }
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    unsafe { std::env::remove_var("ADMIN_PASS"); }
}

#[tokio::test]
async fn fix_c_unlock_login_lists_and_unbans() {
    let _env = lock_admin_env();
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe { std::env::set_var("ADMIN_PASS", "s3cret-admin"); }
    let (state, _tmp) = test_state().await;
    // Unlock: get the HttpOnly 24h cookie.
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"s3cret-admin"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set_cookie.contains("admin="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("Max-Age=86400"), "{set_cookie}");
    // Plain-HTTP server: no `Secure`, or browsers silently drop the cookie.
    assert!(!set_cookie.to_ascii_lowercase().split(';').skip(1).any(|p| p.trim() == "secure"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    // Admin list works with the cookie.
    let resp = router(state.clone())
        .oneshot(get("/api/admin/galleries", &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Unban with garbage key: 400 (valid JSON, unusable key).
    let r = post_json("/api/admin/unban", &[("cookie", cookie.as_str())], r#"{"key":"x"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Unban a well-formed but never-banned key: 404.
    let r = post_json("/api/admin/unban", &[("cookie", cookie.as_str())], r#"{"key":"never-banned-anywhere"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    unsafe { std::env::remove_var("ADMIN_PASS"); }
}

#[tokio::test]
async fn fix_c_expired_admin_token_rejected() {
    let (state, _tmp) = test_state().await;
    let tok = "stale-admin-token".to_string();
    state.admin_tokens.lock().unwrap().insert(
        tok.clone(),
        std::time::Instant::now() - gally_uring::state::ADMIN_TOKEN_TTL - std::time::Duration::from_secs(1),
    );
    let resp = router(state.clone())
        .oneshot(get("/api/admin/galleries", &[("cookie", format!("admin={tok}").as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Expired token is swept on sight.
    assert!(!state.admin_tokens.lock().unwrap().contains_key(&tok));
}

#[tokio::test]
async fn fix_c_oversize_admin_bodies_are_413() {
    let _env = lock_admin_env();
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe { std::env::set_var("ADMIN_PASS", "s3cret-admin"); }
    let (state, _tmp) = test_state().await;
    let cookie = admin_cookie(&state);
    let big = "x".repeat(9000);
    let big_json = format!(r#"{{"password":"{big}"}}"#);
    let r = post_json("/api/admin/unlock", &[], &big_json);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r = post_json("/api/admin/unban", &[("cookie", cookie.as_str())], &big_json.replace("password", "key"));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    unsafe { std::env::remove_var("ADMIN_PASS"); }
}

#[tokio::test]
async fn fix_b_svg_sandboxed() {
    let (state, _tmp) = test_state().await;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
    let (status, v) =
        put_photo(&state, "x.svg", Some(KEY_A), None, "image/svg+xml", svg).await;
    assert_eq!(status, StatusCode::CREATED);
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();
    let uri = format!("/photos/{name}");
    let resp = router(state)
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-security-policy").unwrap(),
        "sandbox"
    );
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment"
    );
}

// --- fix: partial uploads, startup sweep, password/key lockout ---

#[tokio::test]
async fn partial_upload_is_invisible_and_cleaned_up() {
    let (state, tmp) = test_state().await;
    let (status, v) = resolve(&state, KEY_A).await;
    assert_eq!(status, StatusCode::OK);
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();

    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::io::Error>>();
    let r = request(
        "PUT",
        "/api/photos/stream.jpg",
        &[key_header(KEY_A), ("x-gallery-slug", slug.as_str())],
        Body::from_stream(rx),
    );
    let up = tokio::spawn({
        let state = state.clone();
        async move { router(state).oneshot(r).await.unwrap() }
    });

    tx.unbounded_send(Ok(jpeg())).unwrap();
    let photos = tmp.path().join("photos");
    let mut saw_part = false;
    for _ in 0..500 {
        let mut rd = tokio::fs::read_dir(&photos).await.unwrap();
        while let Some(ent) = rd.next_entry().await.unwrap() {
            if ent.file_name().to_string_lossy().starts_with(".part-") {
                saw_part = true;
            }
        }
        if saw_part {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(saw_part, "upload should stream into a .part- temp file");
    assert!(!up.is_finished(), "upload must still be in flight");

    // While the body is open, the gallery lists nothing.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    assert_eq!(listed["photos"].as_array().unwrap().len(), 0);

    tx.unbounded_send(Ok(vec![0x11, 0x22])).unwrap();
    drop(tx);
    let resp = up.await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let done = body_json(resp).await;
    assert!(done["name"].as_str().unwrap().starts_with(&format!("{slug}_")));

    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[key_header(KEY_A)]))
        .await
        .unwrap();
    let listed = body_json(resp).await;
    assert_eq!(listed["photos"].as_array().unwrap().len(), 1);
    let mut rd = tokio::fs::read_dir(&photos).await.unwrap();
    let mut files = 0;
    while let Some(ent) = rd.next_entry().await.unwrap() {
        let name = ent.file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(".part-"), "stale temp file left: {name}");
        files += 1;
    }
    assert_eq!(files, 1);
}

#[tokio::test]
async fn startup_sweeps_stale_part_files() {
    let tmp = tempfile::tempdir().unwrap();
    let photos = tmp.path().join("photos");
    tokio::fs::create_dir_all(&photos).await.unwrap();
    let stale = photos.join(".part-dead");
    tokio::fs::write(&stale, b"junk").await.unwrap();
    let _state = AppState::new(tmp.path()).await.expect("AppState::new");
    assert!(!stale.exists(), "startup must sweep stale .part- files");
}

#[tokio::test]
async fn viewer_password_lockout_after_ten_failures() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"s3cret-pass"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let uri = format!("/api/galleries/{slug}/photos?pass=nope");
    for _ in 0..10 {
        let resp = router(state.clone()).oneshot(get(&uri, &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = router(state.clone()).oneshot(get(&uri, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // A correct password is not hashed (nor accepted) once the bucket is full.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos?pass=s3cret-pass"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn delete_key_lockout_after_ten_failures() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();
    let uri = format!("/api/photos/{name}");
    for _ in 0..10 {
        let r = request("DELETE", &uri, &[key_header(KEY_B)], Body::empty());
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    let r = request("DELETE", &uri, &[key_header(KEY_B)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // The real key still works: lockout only stops guessing.
    let r = request("DELETE", &uri, &[key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// --- private by default + owner media cookie ---

#[tokio::test]
async fn new_gallery_is_private_until_published() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "secret.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    let listing = format!("/api/galleries/{slug}/photos");
    let photo = format!("/photos/{name}");

    // No credential: the gallery does not exist for viewers.
    let resp = router(state.clone()).oneshot(get(&listing, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = router(state.clone()).oneshot(get(&photo, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Owner key: 200, public:false, and a media cookie for <img> tags.
    let resp = router(state.clone())
        .oneshot(get(&listing, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("owner cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains(&format!("galowner_{slug}=")), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    let v = body_json(resp).await;
    assert_eq!(v["public"], serde_json::Value::Bool(false));

    // That cookie alone authorizes the photo (no header needed).
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let resp = router(state.clone())
        .oneshot(get(&photo, &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A forged cookie is not an owner.
    let resp = router(state.clone())
        .oneshot(get(&photo, &[("cookie", "galowner_x=deadbeef")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // A cookie for another slug does not transfer.
    let resp = router(state.clone())
        .oneshot(get(
            &photo,
            &[("cookie", format!("galowner_quiet-river-calm-pond-1a2b={}", &cookie[20..]).as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn visibility_endpoint_publishes_and_hides() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let listing = format!("/api/galleries/{slug}/photos");
    let uri = format!("/api/galleries/{slug}/visibility");

    // No key: 401. Wrong key: 403. Missing field: 400.
    let r = post_json(&uri, &[], r#"{"public":true}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let r = post_json(&uri, &[key_header(KEY_B)], r#"{"public":true}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Publish: viewers can list it, the response echoes the state.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"public":true}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["public"], serde_json::Value::Bool(true));
    let resp = router(state.clone()).oneshot(get(&listing, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["public"], serde_json::Value::Bool(true));

    // Hide again: back to invisible.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"public":false}"#);
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let resp = router(state.clone()).oneshot(get(&listing, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn photo_cache_control_never_public_for_owners() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    let photo = format!("/photos/{name}");

    // Private gallery, owner key: no shared cache may keep the bytes.
    let resp = router(state.clone())
        .oneshot(get(&photo, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("cache-control").unwrap(), "private, no-store");

    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    // Public gallery, anonymous viewer: cacheable.
    let resp = router(state.clone()).oneshot(get(&photo, &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );
    // Owner key on a public gallery still stays out of shared caches.
    let resp = router(state.clone())
        .oneshot(get(&photo, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("cache-control").unwrap(), "private, no-store");
}

// --- quotas, free space, read limit ---

#[tokio::test]
async fn gallery_byte_quota_rejects_and_recovers() {
    let (state, _tmp) =
        test_state_with(Limits { gallery_bytes: 60, ..Limits::defaults() }).await;
    let (status, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", vec![7u8; 40]).await;
    assert_eq!(status, StatusCode::CREATED);
    let slug = v["slug"].as_str().unwrap().to_string();
    let first = v["name"].as_str().unwrap().to_string();
    // Second 40 bytes would exceed 60: refused, nothing stored.
    let (status, _) =
        put_photo(&state, "b.jpg", Some(KEY_A), Some(&slug), "image/jpeg", vec![7u8; 40]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let files: usize = std::fs::read_dir(state.dir.clone()).unwrap().count();
    assert_eq!(files, 1, "oversize upload must not leave a file");
    let meta = state.db.lock().unwrap().gallery_meta(&slug).unwrap();
    assert_eq!((meta.bytes, meta.photos), (40, 1));
    // Deleting frees the space again.
    let r = request("DELETE", &format!("/api/photos/{first}"), &[key_header(KEY_A)], Body::empty());
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let (status, _) =
        put_photo(&state, "c.jpg", Some(KEY_A), Some(&slug), "image/jpeg", vec![7u8; 40]).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn gallery_photo_count_quota() {
    let (state, _tmp) =
        test_state_with(Limits { gallery_photos: 2, ..Limits::defaults() }).await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", b"a".to_vec()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let (status, _) =
        put_photo(&state, "b.jpg", Some(KEY_A), Some(&slug), "image/jpeg", b"b".to_vec()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) =
        put_photo(&state, "c.jpg", Some(KEY_A), Some(&slug), "image/jpeg", b"c".to_vec()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(v["error"], serde_json::Value::String("photo limit reached".into()));
}

#[tokio::test]
async fn min_free_bytes_guard_is_507() {
    // Demand more free space than any volume has.
    let (state, _tmp) =
        test_state_with(Limits { min_free: u64::MAX, ..Limits::defaults() }).await;
    let (status, v) =
        put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", b"x".to_vec()).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(v["error"], serde_json::Value::String("storage full".into()));
    assert_eq!(std::fs::read_dir(state.dir.clone()).unwrap().count(), 0);
}

#[tokio::test]
async fn read_limit_trips_third_request() {
    let (state, _tmp) =
        test_state_with(Limits { read_max: 2, ..Limits::defaults() }).await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let listing = format!("/api/galleries/{slug}/photos");
    for _ in 0..2 {
        let resp = router(state.clone())
            .oneshot(get(&listing, &[key_header(KEY_A)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let resp = router(state.clone())
        .oneshot(get(&listing, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// --- non-ASCII viewer passwords + wrong-key lockout ---

#[tokio::test]
async fn non_ascii_viewer_password_works() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"päss"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    // UTF-8 percent-encoded query, raw UTF-8 header: both accepted.
    for req in [
        get(&format!("/api/galleries/{slug}/photos?pass=p%C3%A4ss"), &[]),
        get(&format!("/api/galleries/{slug}/photos"), &[("x-gallery-pass", "päss")]),
        get(&format!("/photos/{name}?pass=p%C3%A4ss"), &[]),
    ] {
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Invalid UTF-8 in the query cannot match.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos?pass=%FF%FE"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_key_on_locked_gallery_locks_out() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"s3cret-pass"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let listing = format!("/api/galleries/{slug}/photos");
    for _ in 0..10 {
        let resp = router(state.clone())
            .oneshot(get(&listing, &[key_header(KEY_B)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = router(state.clone())
        .oneshot(get(&listing, &[key_header(KEY_B)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// --- range + disposition ---

#[tokio::test]
async fn range_requests_slice_the_file() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "big.jpg", Some(KEY_A), None, "image/jpeg", vec![9u8; 1000]).await;
    let name = v["name"].as_str().unwrap().to_string();
    let uri = format!("/photos/{name}");

    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=10-19")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 10-19/1000"
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(body_bytes(resp).await.len(), 10);

    // Suffix range: last 10 bytes.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=-10")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 990-999/1000"
    );

    // Out of range: 416 with the real length, no body.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=9999-")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes */1000"
    );
    assert!(body_bytes(resp).await.is_empty());

    // Malformed spec: 416 too, never a silent full body.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=abc-def")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Multi-range: served whole (a media probe, not an error).
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=0-9,20-29")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await.len(), 1000);

    // No range: full body + Accept-Ranges.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(body_bytes(resp).await.len(), 1000);
}

#[tokio::test]
async fn non_media_downloads_as_attachment() {
    let (state, _tmp) = test_state().await;
    let (_, pdf) = put_photo(&state, "doc.pdf", Some(KEY_A), None, "application/pdf", b"%PDF".to_vec()).await;
    let (_, txt) = put_photo(&state, "note.txt", Some(KEY_A), None, "text/plain", b"hi".to_vec()).await;
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{}", pdf["name"].as_str().unwrap()),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("content-disposition").unwrap(), "attachment");
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{}", txt["name"].as_str().unwrap()),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert!(resp.headers().get("content-disposition").is_none(), "text stays inline");
}

// --- admin abuse tooling ---

/// Delete a gallery by slug as admin; returns (status, json).
async fn admin_delete(state: &Arc<AppState>, cookie: &str, slug: &str) -> (StatusCode, serde_json::Value) {
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug}/delete"),
        &[("cookie", cookie)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    let s = resp.status();
    (s, body_json(resp).await)
}

#[tokio::test]
async fn ban_sweep_removes_every_gallery_of_the_key() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug_a = a["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    let (_, b) = put_photo(&state, "b.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug_b = b["slug"].as_str().unwrap().to_string();
    let name_b = b["name"].as_str().unwrap().to_string();

    let cookie = admin_cookie(&state);
    let (status, v) = admin_delete(&state, &cookie, &slug_a).await;
    assert_eq!(status, StatusCode::OK);
    let slugs: Vec<&str> = v["slugs"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert_eq!(slugs.len(), 2, "{v}");
    assert!(slugs.contains(&slug_a.as_str()) && slugs.contains(&slug_b.as_str()));
    assert_eq!(v["files"], serde_json::Value::from(2));
    // Both files and both rows are gone.
    assert!(!state.dir.join(&name_a).exists());
    assert!(!state.dir.join(&name_b).exists());
    assert!(!state.db.lock().unwrap().gallery_exists(&slug_a));
    assert!(!state.db.lock().unwrap().gallery_exists(&slug_b));

    // The ban is listed with a readable label.
    let resp = router(state.clone())
        .oneshot(get("/api/admin/banned", &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    let v = body_json(resp).await;
    let first = &v["banned"][0];
    assert!(first["label"].as_str().is_some(), "{v}");

    // Unknown slug: 404.
    let (status, _) = admin_delete(&state, &cookie, "quiet-river-calm-pond-1a2b").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_remove_deletes_one_gallery_and_keeps_the_key() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug_a = a["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    let (_, b) = put_photo(&state, "b.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug_b = b["slug"].as_str().unwrap().to_string();

    let cookie = admin_cookie(&state);
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug_a}/remove"),
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["files"], serde_json::Value::from(1));
    assert!(!state.dir.join(&name_a).exists());
    assert!(!state.db.lock().unwrap().gallery_exists(&slug_a));
    assert!(state.db.lock().unwrap().gallery_exists(&slug_b));
    // Key still works: the gallery is not banned.
    let (status, _) =
        put_photo(&state, "c.jpg", Some(KEY_A), Some(&slug_b), "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::CREATED);
    // Non-admin refused.
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug_a}/remove"),
        &[],
        Body::empty(),
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn admin_sweep_removes_only_orphans() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "keep.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let kept = v["name"].as_str().unwrap().to_string();
    let hex = "c".repeat(32);
    let orphan = format!("quiet-river-calm-pond-1a2b_{hex}_junk.jpg");
    std::fs::write(state.dir.join(&orphan), b"junk").unwrap();

    let cookie = admin_cookie(&state);
    let r = request("POST", "/api/admin/sweep", &[("cookie", cookie.as_str())], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["deleted"], serde_json::Value::from(1));
    assert_eq!(v["failed"], serde_json::Value::from(0));
    assert!(!state.dir.join(&orphan).exists());
    assert!(state.dir.join(&kept).exists());
}

#[tokio::test]
async fn admin_logout_clears_cookie_and_token() {
    let (state, _tmp) = test_state().await;
    let cookie = admin_cookie(&state);
    let r = request("POST", "/api/admin/logout", &[("cookie", cookie.as_str())], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
    // The revoked token no longer works.
    let resp = router(state.clone())
        .oneshot(get("/api/admin/galleries", &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Not an admin: refused.
    let r = request("POST", "/api/admin/logout", &[], Body::empty());
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn security_headers_on_every_response() {
    let (state, _tmp) = test_state().await;
    for uri in ["/app.js", "/gally/admin/", "/api/admin/galleries"] {
        let resp = router(state.clone()).oneshot(get(uri, &[])).await.unwrap();
        let h = resp.headers();
        assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff", "{uri}");
        assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer", "{uri}");
        assert_eq!(h.get("x-frame-options").unwrap(), "DENY", "{uri}");
        assert_eq!(
            h.get("content-security-policy").unwrap(),
            "frame-ancestors 'none'",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn admin_unlock_lockout_survives_unlimited_rate() {
    let _env = lock_admin_env();
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe { std::env::set_var("ADMIN_PASS", "lockout-admin"); }
    // SAFETY: ADMIN_ENV_LOCK is held; GALLY_RATE_MAX is also process-global.
    unsafe { std::env::set_var("GALLY_RATE_MAX", "0"); }
    let (state, _tmp) = test_state().await;
    for _ in 0..10 {
        let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    // 11th wrong guess trips the failure lockout even with uploads unlimited.
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    unsafe { std::env::remove_var("GALLY_RATE_MAX"); }
    unsafe { std::env::remove_var("ADMIN_PASS"); }
}

#[tokio::test]
async fn wrong_key_on_private_listing_locks_out() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let uri = format!("/api/galleries/{slug}/photos");
    for _ in 0..10 {
        let resp = router(state.clone())
            .oneshot(get(&uri, &[key_header(KEY_B)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_B)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // The real key still works: lockout only stops guessing.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn resolve_with_header_key_and_body_slug_resolves() {
    let (state, _tmp) = test_state().await;
    let (_, v) = resolve(&state, KEY_A).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    // Header key + body slug: must resolve, not mint a stray gallery.
    let r = post_json(
        "/api/galleries/resolve",
        &[key_header(KEY_A)],
        &format!(r#"{{"slug":{slug:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["slug"].as_str(), Some(slug.as_str()));
    // Present-but-invalid slug: 400, never a fresh gallery.
    let r = post_json(
        "/api/galleries/resolve",
        &[key_header(KEY_A)],
        r#"{"slug":"bogus!!!"}"#,
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invalid_slug_header_is_400_not_mint() {
    let (state, _tmp) = test_state().await;
    let (status, _) =
        put_photo(&state, "x.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::CREATED);
    // Hand-rolled PUT with a mistyped slug header: 400, no new gallery.
    let mut h = vec![("content-type", "image/jpeg"), key_header(KEY_A)];
    h.push(("x-gallery-slug", "Happy-Otter-Bogus"));
    let r = request("PUT", "/api/photos/x.jpg", &h, Body::from(jpeg()));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// --- listing lock state + failed-mint cleanup ---

#[tokio::test]
async fn listing_reports_lock_state() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "p.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let uri = format!("/api/galleries/{slug}/photos");

    let resp = router(state.clone()).oneshot(get(&uri, &[key_header(KEY_A)])).await.unwrap();
    assert_eq!(body_json(resp).await["locked"], serde_json::Value::Bool(false));

    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"pw-1234"}"#,
    );
    assert_eq!(router(state.clone()).oneshot(r).await.unwrap().status(), StatusCode::OK);

    // The owner still sees the gallery, and must be told it is locked: an
    // unauthenticated probe cannot (private galleries answer 404 first).
    let resp = router(state.clone()).oneshot(get(&uri, &[key_header(KEY_A)])).await.unwrap();
    assert_eq!(body_json(resp).await["locked"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn failed_home_upload_leaves_no_gallery() {
    let (state, _tmp) = test_state().await;
    // A home PUT mints before the body is read; an empty body must not leave
    // an empty gallery behind in the admin list.
    let (status, _) = put_photo(&state, "empty.jpg", Some(KEY_A), None, "image/jpeg", Vec::new()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rows: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM galleries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "minted gallery must be rolled back");
}
