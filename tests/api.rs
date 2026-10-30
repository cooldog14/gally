use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use gally_uring::{router, validate::is_slug, AppState};
use tower::ServiceExt;

const KEY_A: &str = "test-key-alpha";
const KEY_B: &str = "test-key-other";

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = AppState::new(tmp.path()).await.expect("AppState::new");
    (state, tmp)
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
    state.admin_tokens.lock().unwrap().insert(tok.clone());
    format!("admin={tok}")
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
    let mut h = vec![("content-type", "image/jpeg"), key_header(KEY_A), invite_header("nope")];
    let uri = "/api/photos/gated.jpg".to_string();
    let r = request("PUT", &uri, &h, Body::from(jpeg()));
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Right invite: gallery mints.
    let mut h = vec![
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
        .oneshot(get(&uri, &[]))
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
        .oneshot(get(&uri, &[]))
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
    let resp = router(state).oneshot(get(&uri, &[])).await.unwrap();
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
    // SAFETY: single-threaded test binary; no other test reads GALLY_MAX_BYTES concurrently.
    unsafe { std::env::set_var("GALLY_MAX_BYTES", "16"); }
    let (state, _tmp) = test_state().await;
    let big = vec![0u8; 17];
    let (status, _) = put_photo(&state, "big.jpg", Some(KEY_A), None, "image/jpeg", big).await;
    unsafe { std::env::remove_var("GALLY_MAX_BYTES"); }
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

    // Gallery row stays: empty listing is 200 with no photos.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[]))
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
        .oneshot(get(&format!("/api/galleries/{slug}/photos"), &[]))
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

    // Still there.
    let uri = format!("/photos/{name}");
    let resp = router(state).oneshot(get(&uri, &[])).await.unwrap();
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
    // Only this test touches the process env, and no other test reads
    // ADMIN_PASS, so remove/restore here is race-free within this binary.
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
async fn compat_register_is_404() {
    let (state, _tmp) = test_state().await;
    let r = post_json("/api/register", &[], "{}");
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_me_is_401() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/api/me", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
