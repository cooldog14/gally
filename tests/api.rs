use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use gally_uring::db::GalleryDb;
use gally_uring::state::{MediaToken, media_token};
use gally_uring::{AppState, router, state::Limits, validate::is_slug};
use tower::ServiceExt;

const KEY_A: &str = "test-key-alpha";
const KEY_B: &str = "test-key-other";

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    test_state_with(Limits::defaults()).await
}

async fn test_state_with(limits: Limits) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = AppState::new_with(tmp.path(), limits)
        .await
        .expect("AppState::new_with");
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
    state
        .admin_tokens
        .lock()
        .unwrap()
        .insert(tok.clone(), std::time::Instant::now());
    format!("admin={tok}")
}

/// `ADMIN_PASS` is process-global: every test that sets/removes it must hold
/// this lock for its whole body, or parallel tests race each other.
static ADMIN_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
async fn lock_admin_env() -> tokio::sync::MutexGuard<'static, ()> {
    ADMIN_ENV_LOCK.lock().await
}

fn key_header(key: &str) -> (&str, &str) {
    ("x-gallery-key", key)
}

/// `name=value` pair of the first `Set-Cookie` whose name starts with
/// `prefix`, ready to send back as a `Cookie` header.
fn set_cookie_pair(resp: &Response, prefix: &str) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with(prefix))
        .map(|c| c.split(';').next().unwrap().to_string())
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

/// Minimal JPEG whose EXIF APP1 segment carries one `DateTimeOriginal`.
fn jpeg_with_exif(dt: &str) -> Vec<u8> {
    use exif::{In, Tag, Value};
    let field = exif::Field {
        tag: Tag::DateTimeOriginal,
        ifd_num: In::PRIMARY,
        value: Value::Ascii(vec![dt.as_bytes().to_vec()]),
    };
    let mut writer = exif::experimental::Writer::new();
    writer.push_field(&field);
    let mut tiff = std::io::Cursor::new(Vec::new());
    writer.write(&mut tiff, true).expect("write EXIF");
    let tiff = tiff.into_inner();

    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    // APP1 length covers itself, "Exif\0\0", and the TIFF block.
    jpeg.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    jpeg.extend_from_slice(b"Exif\0\0");
    jpeg.extend_from_slice(&tiff);
    jpeg.extend_from_slice(&[0xFF, 0xD9]);
    jpeg
}

/// One resumable-upload request: `range` is `bytes START-END/TOTAL`.
async fn put_chunk(
    state: &Arc<AppState>,
    name: &str,
    key: &str,
    slug: &str,
    id: &str,
    range: &str,
    bytes: &[u8],
) -> (StatusCode, serde_json::Value) {
    let r = request(
        "PUT",
        &format!("/api/photos/{name}"),
        &[
            ("content-type", "image/jpeg"),
            ("x-gallery-key", key),
            ("x-gallery-slug", slug),
            ("x-upload-id", id),
            ("content-range", range),
        ],
        Body::from(bytes.to_vec()),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// A real (decodable) PNG, needed for thumbnail generation.
fn png() -> Vec<u8> {
    use image::ImageEncoder;
    let img = image::RgbImage::from_pixel(8, 8, image::Rgb([180, 30, 40]));
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
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
            .oneshot(get(
                &format!("/api/galleries/{s}/photos"),
                &[key_header(KEY_A)],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Explicit slug header lands in that gallery (gallery-page upload).
    let r = request(
        "PUT",
        "/api/photos/stray.jpg",
        &[
            ("content-type", "image/jpeg"),
            key_header(KEY_A),
            ("x-gallery-slug", s1),
        ],
        Body::from(jpeg()),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Unknown-slug header: rejected, never minted.
    let r = request(
        "PUT",
        "/api/photos/stray2.jpg",
        &[
            ("content-type", "image/jpeg"),
            key_header(KEY_A),
            ("x-gallery-slug", "quiet-river-calm-pond-1a2b"),
        ],
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
    state
        .invite
        .lock()
        .unwrap()
        .replace("amber-fox-glade-ridge".to_string());

    // No invite: creation refused, nothing stored.
    let (status, v) = put_photo(&state, "gated.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        v.get("error").and_then(|e| e.as_str()),
        Some("invite required")
    );

    // Wrong invite: still refused.
    let h = vec![
        ("content-type", "image/jpeg"),
        key_header(KEY_A),
        invite_header("nope"),
    ];
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
    let first_slug = body_json(resp)
        .await
        .get("slug")
        .and_then(|s| s.as_str())
        .unwrap()
        .to_string();

    // Existing gallery: contribution targets the slug, needs no invite.
    let (status, v) = put_photo(
        &state,
        "second.jpg",
        Some(KEY_A),
        Some(&first_slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        v.get("slug").and_then(|s| s.as_str()),
        Some(first_slug.as_str())
    );
}

#[tokio::test]
async fn resolve_needs_invite_for_new_gallery() {
    let (state, _tmp) = test_state().await;
    state
        .invite
        .lock()
        .unwrap()
        .replace("amber-fox-glade-ridge".to_string());

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
    let (status, v) = put_photo(
        &state,
        "pic.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
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
    let (status, v) = put_photo(
        &state,
        "shot.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        bytes.clone(),
    )
    .await;
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
    let (status, v) = put_photo(
        &state,
        "shot.jpg",
        Some(KEY_A),
        None,
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        v.get("name")
            .and_then(|n| n.as_str())
            .unwrap()
            .ends_with("shot.jpg")
    );
}

#[tokio::test]
async fn put_any_extension_accepted() {
    let (state, _tmp) = test_state().await;
    let (status, v) = put_photo(
        &state,
        "notes.txt",
        Some(KEY_A),
        None,
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        v.get("name")
            .and_then(|n| n.as_str())
            .unwrap()
            .ends_with("notes.txt")
    );
}

#[tokio::test]
async fn put_oversize_body_is_413() {
    // Limits are per-state now: no process-wide env to race with.
    let (state, _tmp) = test_state_with(Limits {
        max_file: 16,
        ..Limits::defaults()
    })
    .await;
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
    assert_eq!(
        body_json(resp).await.get("gallery_gone"),
        Some(&serde_json::Value::Bool(false))
    );

    // Gallery row stays: owner's empty listing is 200 with no photos.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router(state)
        .oneshot(get(&format!("/{slug}"), &[]))
        .await
        .unwrap();
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
    assert_eq!(
        body_json(resp).await.get("locked"),
        Some(&serde_json::Value::Bool(true))
    );
    // Wrong password still 401.
    let resp = router(state.clone())
        .oneshot(get(&format!("/api/galleries/{slug}/photos?pass=nope"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // Right password via query: 200.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=s3cret-pass"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Owner key bypasses without password.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
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
async fn unlock_sets_viewer_cookie_that_serves_media() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
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

    // Unlock through the listing: the 200 carries the viewer cookie.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=s3cret-pass"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("viewer cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        set_cookie.starts_with(&format!("galpass_{slug}=")),
        "{set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/photos/"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    // Cookie alone serves the photo: no ?pass=, no key. Still never cached.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{name}"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "private, no-store"
    );
    assert_eq!(body_bytes(resp).await.len(), jpeg().len());
}

#[tokio::test]
async fn viewer_cookie_is_bound_to_its_gallery() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let (_, b) = put_photo(&state, "b.jpg", Some(KEY_B), None, "image/jpeg", jpeg()).await;
    let slug_a = a["slug"].as_str().unwrap().to_string();
    let slug_b = b["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    for (slug, key) in [(&slug_a, KEY_A), (&slug_b, KEY_B)] {
        let r = post_json(
            &format!("/api/galleries/{slug}/visibility"),
            &[key_header(key)],
            r#"{"public":true}"#,
        );
        assert_eq!(
            router(state.clone()).oneshot(r).await.unwrap().status(),
            StatusCode::OK
        );
        let r = post_json(
            &format!("/api/galleries/{slug}/lock"),
            &[key_header(key)],
            r#"{"password":"only-a"}"#,
        );
        assert_eq!(
            router(state.clone()).oneshot(r).await.unwrap().status(),
            StatusCode::OK
        );
    }
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug_a}/photos?pass=only-a"),
            &[],
        ))
        .await
        .unwrap();
    let pair = set_cookie_pair(&resp, &format!("galpass_{slug_a}=")).expect("viewer cookie");
    let tok = pair.split_once('=').unwrap().1;

    // Gallery A's token relabelled for gallery B opens nothing.
    let wrong = format!("galpass_{slug_b}={tok}");
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug_b}/photos"),
            &[("cookie", wrong.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Nor may a viewer token pose as an owner cookie: separate token kinds.
    let forged = format!("galowner_{slug_a}={tok}");
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{name_a}"),
            &[("cookie", forged.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn viewer_cookie_is_revoked_when_password_changes() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let uri = format!("/api/galleries/{slug}/lock");
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"password":"old-pass"}"#);
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );

    // Unlock through the listing to mint a viewer cookie for the old password.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=old-pass"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = set_cookie_pair(&resp, &format!("galpass_{slug}=")).expect("viewer cookie");
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{name}"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Changing the password must invalidate the old unlock.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"password":"new-pass"}"#);
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    // Explicit cookie header: the real cookie's Path=/photos/ would not be
    // sent to the listing API by a browser jar.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{name}"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // The new password still unlocks.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=new-pass"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn photo_etag_revalidates_with_304() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v["name"].as_str().unwrap().to_string();
    let photo = format!("/photos/{name}");

    let resp = router(state.clone())
        .oneshot(get(&photo, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get("etag")
        .expect("photo etag")
        .to_str()
        .unwrap()
        .to_string();

    // Same ETag: 304, empty body, header unchanged.
    let resp = router(state.clone())
        .oneshot(get(
            &photo,
            &[key_header(KEY_A), ("if-none-match", etag.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers().get("etag").unwrap(), etag.as_str());
    assert!(body_bytes(resp).await.is_empty());

    // Range requests are never 304: they still slice.
    let resp = router(state.clone())
        .oneshot(get(
            &photo,
            &[
                key_header(KEY_A),
                ("range", "bytes=0-3"),
                ("if-none-match", etag.as_str()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn delete_keeps_gallery_when_photos_remain() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "one.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v.get("slug").and_then(|s| s.as_str()).unwrap().to_string();
    let (_, v2) = put_photo(
        &state,
        "two.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    let name2 = v2.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name2}");
    let r = request("DELETE", &uri, &[key_header(KEY_A)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await.get("gallery_gone"),
        Some(&serde_json::Value::Bool(false))
    );

    let resp = router(state)
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
#[tokio::test]
async fn delete_by_stranger_is_404() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap().to_string();

    let uri = format!("/api/photos/{name}");
    let r = request("DELETE", &uri, &[key_header(KEY_B)], Body::empty());
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    // Unknown and not-yours answer identically (404): no existence oracle.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

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
    let _env = lock_admin_env().await;
    // Only tests holding ADMIN_ENV_LOCK touch the process env, so
    // remove/restore here is race-free within this binary.
    let saved = std::env::var("ADMIN_PASS").ok();
    // SAFETY: only this test touches the process env in this binary.
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
    let (state, _tmp) = test_state().await;
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"x"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    if let Some(v) = saved {
        // SAFETY: same as above.
        unsafe {
            std::env::set_var("ADMIN_PASS", v);
        }
    }
}
#[tokio::test]
async fn admin_endpoints_reject_unauth() {
    let (state, _tmp) = test_state().await;
    for uri in ["/api/admin/galleries"] {
        let resp = router(state.clone()).oneshot(get(uri, &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn admin_endpoints_accept_token() {
    let (state, _tmp) = test_state().await;
    let cookie = admin_cookie(&state);
    for uri in ["/api/admin/galleries"] {
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
    let resp = router(state)
        .oneshot(get("/gally/admin/", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ctype.starts_with("text/html"), "{ctype}");
    let body = body_bytes(resp).await;
    assert!(
        body.windows(b"/api/admin/unlock".len())
            .any(|w| w == b"/api/admin/unlock")
    );
}

#[tokio::test]
async fn admin_page_redirect_no_slash() {
    let (state, _tmp) = test_state().await;
    let resp = router(state)
        .oneshot(get("/gally/admin", &[]))
        .await
        .unwrap();
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
    let resp = router(state)
        .oneshot(get(&format!("/{slug}"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ctype.starts_with("text/html"), "{ctype}");
}

#[tokio::test]
async fn unknown_gallery_slug_redirects_home() {
    let (state, _tmp) = test_state().await;
    let resp = router(state)
        .oneshot(get("/quiet-river-calm-pond-1a2b", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers().get("location").unwrap(), "/");
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
    let r = request(
        "POST",
        &uri,
        &[("content-type", "application/json"), key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn fix_c_wrong_admin_password_is_403() {
    let _env = lock_admin_env().await;
    let (state, _tmp) = test_state().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe {
        std::env::set_var("ADMIN_PASS", "correct-horse");
    }
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
}

#[tokio::test]
async fn admin_unlock_lists_and_removes() {
    let _env = lock_admin_env().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe {
        std::env::set_var("ADMIN_PASS", "s3cret-admin");
    }
    let (state, _tmp) = test_state().await;
    // Unlock: get the HttpOnly 24h cookie.
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"s3cret-admin"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("admin="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("Max-Age=86400"), "{set_cookie}");
    // Plain-HTTP server: no `Secure`, or browsers silently drop the cookie.
    assert!(
        !set_cookie
            .to_ascii_lowercase()
            .split(';')
            .skip(1)
            .any(|p| p.trim() == "secure"),
        "{set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    // Upload so the list shows a gallery, then remove it via admin.
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let resp = router(state.clone())
        .oneshot(get("/api/admin/galleries", &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await["galleries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["slug"] == slug.as_str())
    );
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug}/remove"),
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!state.db.lock().unwrap().gallery_exists(&slug));
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
}

#[tokio::test]
async fn fix_c_expired_admin_token_rejected() {
    let (state, _tmp) = test_state().await;
    let tok = "stale-admin-token".to_string();
    state.admin_tokens.lock().unwrap().insert(
        tok.clone(),
        std::time::Instant::now()
            - gally_uring::state::ADMIN_TOKEN_TTL
            - std::time::Duration::from_secs(1),
    );
    let resp = router(state.clone())
        .oneshot(get(
            "/api/admin/galleries",
            &[("cookie", format!("admin={tok}").as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Expired token is swept on sight.
    assert!(!state.admin_tokens.lock().unwrap().contains_key(&tok));
}

#[tokio::test]
async fn fix_c_oversize_admin_bodies_are_413() {
    let _env = lock_admin_env().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe {
        std::env::set_var("ADMIN_PASS", "s3cret-admin");
    }
    let (state, _tmp) = test_state().await;
    let big = "x".repeat(9000);
    let big_json = format!(r#"{{"password":"{big}"}}"#);
    let r = post_json("/api/admin/unlock", &[], &big_json);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
}

#[tokio::test]
async fn fix_b_svg_sandboxed() {
    let (state, _tmp) = test_state().await;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
    let (status, v) = put_photo(&state, "x.svg", Some(KEY_A), None, "image/svg+xml", svg).await;
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
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
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
    assert!(
        done["name"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{slug}_"))
    );

    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
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
    let thumbs = photos.join(".thumbs");
    tokio::fs::create_dir_all(&thumbs).await.unwrap();
    let stale_thumb = thumbs.join(".tmp-dead.jpg");
    tokio::fs::write(&stale_thumb, b"junk").await.unwrap();
    let kept_thumb = thumbs.join("keep.jpg");
    tokio::fs::write(&kept_thumb, b"junk").await.unwrap();
    let _state = AppState::new(tmp.path()).await.expect("AppState::new");
    assert!(!stale.exists(), "startup must sweep stale .part- files");
    assert!(
        !stale_thumb.exists(),
        "startup must sweep stale thumbnail temps"
    );
    assert!(kept_thumb.exists(), "real thumbs are never swept");
}

#[tokio::test]
async fn second_instance_cannot_share_the_data_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let first = AppState::new_with(tmp.path(), Limits::defaults())
        .await
        .expect("first instance");
    // AppState has no Debug, so check is_err() rather than unwrap_err().
    assert!(
        AppState::new_with(tmp.path(), Limits::defaults())
            .await
            .is_err(),
        "a second instance must refuse the same data dir"
    );
    drop(first);
    AppState::new_with(tmp.path(), Limits::defaults())
        .await
        .expect("lock released with the first instance");
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
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=s3cret-pass"),
            &[],
        ))
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
        // Unknown and not-yours answer identically (404): no existence oracle.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
    let (_, v) = put_photo(
        &state,
        "secret.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        jpeg(),
    )
    .await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    let listing = format!("/api/galleries/{slug}/photos");
    let photo = format!("/photos/{name}");

    // No credential: the gallery does not exist for viewers.
    let resp = router(state.clone())
        .oneshot(get(&listing, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = router(state.clone())
        .oneshot(get(&photo, &[]))
        .await
        .unwrap();
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
    assert!(
        set_cookie.contains(&format!("galowner_{slug}=")),
        "{set_cookie}"
    );
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
            &[(
                "cookie",
                format!("galowner_quiet-river-calm-pond-1a2b={}", &cookie[20..]).as_str(),
            )],
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
    let resp = router(state.clone())
        .oneshot(get(&listing, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await["public"],
        serde_json::Value::Bool(true)
    );

    // Hide again: back to invisible.
    let r = post_json(&uri, &[key_header(KEY_A)], r#"{"public":false}"#);
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let resp = router(state.clone())
        .oneshot(get(&listing, &[]))
        .await
        .unwrap();
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
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "private, no-store"
    );

    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    // Public gallery, anonymous viewer: cacheable.
    let resp = router(state.clone())
        .oneshot(get(&photo, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=86400"
    );
    // Owner key on a public gallery still stays out of shared caches.
    let resp = router(state.clone())
        .oneshot(get(&photo, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "private, no-store"
    );
}

// --- quotas, free space, read limit ---

#[tokio::test]
async fn gallery_byte_quota_rejects_and_recovers() {
    let (state, _tmp) = test_state_with(Limits {
        gallery_bytes: 60,
        ..Limits::defaults()
    })
    .await;
    let (status, v) = put_photo(
        &state,
        "a.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        vec![7u8; 40],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let slug = v["slug"].as_str().unwrap().to_string();
    let first = v["name"].as_str().unwrap().to_string();
    // Second 40 bytes would exceed 60: refused, nothing stored.
    let (status, _) = put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        vec![7u8; 40],
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let files: usize = std::fs::read_dir(state.dir.clone()).unwrap().count();
    assert_eq!(files, 1, "oversize upload must not leave a file");
    let meta = state.db.lock().unwrap().gallery_meta(&slug).unwrap();
    assert_eq!((meta.bytes, meta.photos), (40, 1));
    // Deleting frees the space again.
    let r = request(
        "DELETE",
        &format!("/api/photos/{first}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let (status, _) = put_photo(
        &state,
        "c.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        vec![7u8; 40],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn gallery_photo_count_quota() {
    let (state, _tmp) = test_state_with(Limits {
        gallery_photos: 2,
        ..Limits::defaults()
    })
    .await;
    let (_, v) = put_photo(
        &state,
        "a.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        b"a".to_vec(),
    )
    .await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let (status, _) = put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        b"b".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = put_photo(
        &state,
        "c.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        b"c".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        v["error"],
        serde_json::Value::String("photo limit reached".into())
    );
}

#[tokio::test]
async fn min_free_bytes_guard_is_507() {
    // Demand more free space than any volume has.
    let (state, _tmp) = test_state_with(Limits {
        min_free: u64::MAX,
        ..Limits::defaults()
    })
    .await;
    let (status, v) = put_photo(
        &state,
        "a.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(v["error"], serde_json::Value::String("storage full".into()));
    assert_eq!(std::fs::read_dir(state.dir.clone()).unwrap().count(), 0);
}

#[tokio::test]
async fn read_limit_trips_third_request() {
    let (state, _tmp) = test_state_with(Limits {
        read_max: 2,
        ..Limits::defaults()
    })
    .await;
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
    // Percent-encoded UTF-8 in query or header, and a raw UTF-8 header: all
    // accepted (app.js sends the header percent-encoded).
    for req in [
        get(&format!("/api/galleries/{slug}/photos?pass=p%C3%A4ss"), &[]),
        get(
            &format!("/api/galleries/{slug}/photos"),
            &[("x-gallery-pass", "p%C3%A4ss")],
        ),
        get(
            &format!("/api/galleries/{slug}/photos"),
            &[("x-gallery-pass", "päss")],
        ),
        get(&format!("/photos/{name}?pass=p%C3%A4ss"), &[]),
    ] {
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Invalid UTF-8 in the query cannot match.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=%FF%FE"),
            &[],
        ))
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
    let (_, v) = put_photo(
        &state,
        "big.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        vec![9u8; 1000],
    )
    .await;
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
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */1000");
    assert!(body_bytes(resp).await.is_empty());

    // Malformed spec: 416 too, never a silent full body.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A), ("range", "bytes=abc-def")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Multi-range: served whole (a media probe, not an error).
    let resp = router(state.clone())
        .oneshot(get(
            &uri,
            &[key_header(KEY_A), ("range", "bytes=0-9,20-29")],
        ))
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
    let (_, pdf) = put_photo(
        &state,
        "doc.pdf",
        Some(KEY_A),
        None,
        "application/pdf",
        b"%PDF".to_vec(),
    )
    .await;
    let (_, txt) = put_photo(
        &state,
        "note.txt",
        Some(KEY_A),
        None,
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{}", pdf["name"].as_str().unwrap()),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment"
    );
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/{}", txt["name"].as_str().unwrap()),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert!(
        resp.headers().get("content-disposition").is_none(),
        "text stays inline"
    );
}

// --- admin abuse tooling ---

#[tokio::test]
async fn delete_gallery_by_owner_removes_row_and_files() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    let (_, b) = put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    let name_b = b["name"].as_str().unwrap().to_string();
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["files"], serde_json::Value::from(2));
    assert!(!state.dir.join(&name_a).exists());
    assert!(!state.dir.join(&name_b).exists());
    // Row gone: the owner listing answers like an unknown gallery.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Admin list no longer contains the slug.
    let cookie = admin_cookie(&state);
    let resp = router(state.clone())
        .oneshot(get("/api/admin/galleries", &[("cookie", cookie.as_str())]))
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert!(
        !v["galleries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["slug"] == slug.as_str())
    );
}

#[tokio::test]
async fn delete_gallery_by_stranger_is_404_and_keeps_gallery() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_B)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Owner listing still works with both photos listed (second upload reuses slug).
    let (_, b) = put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    assert_eq!(b["slug"].as_str().unwrap(), slug);
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["photos"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delete_unknown_gallery_is_404() {
    let (state, _tmp) = test_state().await;
    for (key, slug) in [
        (KEY_A, "quiet-river-calm-pond-1a2b"),
        (KEY_B, "no-such-gallery-zzz"),
    ] {
        let r = request(
            "DELETE",
            &format!("/api/galleries/{slug}"),
            &[key_header(key)],
            Body::empty(),
        );
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{slug}");
    }
    // No key either: same 404, no oracle.
    let r = request(
        "DELETE",
        "/api/galleries/quiet-river-calm-pond-1a2b",
        &[],
        Body::empty(),
    );
    let resp = router(state).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
    // Key still works after the remove.
    let (status, _) = put_photo(
        &state,
        "c.jpg",
        Some(KEY_A),
        Some(&slug_b),
        "image/jpeg",
        jpeg(),
    )
    .await;
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
async fn delete_gallery_fails_when_photos_dir_is_unreadable() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    // Rename instead of chmod: the failure must also happen when tests run as root.
    let moved = state.dir.parent().unwrap().join("photos-moved");
    std::fs::rename(&state.dir, &moved).expect("rename photos dir away");
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        state.db.lock().unwrap().gallery_exists(&slug),
        "a failed sweep must leave the row intact"
    );
    std::fs::rename(&moved, &state.dir).expect("rename photos dir back");
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!state.dir.join(&name).exists());
}

#[tokio::test]
async fn admin_remove_fails_when_photos_dir_is_unreadable() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    let cookie = admin_cookie(&state);
    let moved = state.dir.parent().unwrap().join("photos-moved");
    std::fs::rename(&state.dir, &moved).expect("rename photos dir away");
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug}/remove"),
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        state.db.lock().unwrap().gallery_exists(&slug),
        "a failed sweep must leave the row intact"
    );
    std::fs::rename(&moved, &state.dir).expect("rename photos dir back");
    let r = request(
        "POST",
        &format!("/api/admin/galleries/{slug}/remove"),
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!state.dir.join(&name).exists());
}

#[tokio::test]
async fn admin_sweep_removes_only_orphans() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "keep.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let kept = v["name"].as_str().unwrap().to_string();
    let hex = "c".repeat(32);
    let orphan = format!("quiet-river-calm-pond-1a2b_{hex}_junk.jpg");
    std::fs::write(state.dir.join(&orphan), b"junk").unwrap();
    let orphan_thumb = state.dir.join(".thumbs").join(format!("{orphan}.jpg"));
    std::fs::create_dir_all(orphan_thumb.parent().unwrap()).unwrap();
    std::fs::write(&orphan_thumb, b"junk").unwrap();

    let cookie = admin_cookie(&state);
    let r = request(
        "POST",
        "/api/admin/sweep",
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["deleted"], serde_json::Value::from(2));
    assert_eq!(v["failed"], serde_json::Value::from(0));
    assert!(!state.dir.join(&orphan).exists());
    assert!(!orphan_thumb.exists());
    assert!(state.dir.join(&kept).exists());
}

#[tokio::test]
async fn admin_logout_clears_cookie_and_token() {
    let (state, _tmp) = test_state().await;
    let cookie = admin_cookie(&state);
    let r = request(
        "POST",
        "/api/admin/logout",
        &[("cookie", cookie.as_str())],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
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
    let _env = lock_admin_env().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe {
        std::env::set_var("ADMIN_PASS", "lockout-admin");
    }
    let (state, _tmp) = test_state_with(Limits {
        rate_max: 0,
        ..Limits::defaults()
    })
    .await;
    for _ in 0..10 {
        let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    // 11th wrong guess trips the failure lockout even with uploads unlimited.
    let r = post_json("/api/admin/unlock", &[], r#"{"password":"wrong"}"#);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
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
    let (status, _) = put_photo(&state, "x.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
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

    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(
        body_json(resp).await["locked"],
        serde_json::Value::Bool(false)
    );

    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"pw-1234"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );

    // The owner still sees the gallery, and must be told it is locked: an
    // unauthenticated probe cannot (private galleries answer 404 first).
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(
        body_json(resp).await["locked"],
        serde_json::Value::Bool(true)
    );
}

#[tokio::test]
async fn failed_home_upload_leaves_no_gallery() {
    let (state, _tmp) = test_state().await;
    // A home PUT mints before the body is read; an empty body must not leave
    // an empty gallery behind in the admin list.
    let (status, _) = put_photo(
        &state,
        "empty.jpg",
        Some(KEY_A),
        None,
        "image/jpeg",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rows: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM galleries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "minted gallery must be rolled back");
}

// --- H1/H2: no existence oracle on upload/delete ---

#[tokio::test]
async fn put_upload_unknown_slug_matches_not_yours() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    // Unknown-slug upload: 404.
    let r = request(
        "PUT",
        "/api/photos/probe.jpg",
        &[
            ("content-type", "image/jpeg"),
            key_header(KEY_A),
            ("x-gallery-slug", "quiet-river-calm-pond-1a2b"),
        ],
        Body::from(jpeg()),
    );
    let unknown = router(state.clone()).oneshot(r).await.unwrap().status();
    assert_eq!(unknown, StatusCode::NOT_FOUND);
    // Existing slug, wrong key: same 404 (not 403).
    let r = request(
        "PUT",
        "/api/photos/probe.jpg",
        &[
            ("content-type", "image/jpeg"),
            key_header(KEY_B),
            ("x-gallery-slug", slug.as_str()),
        ],
        Body::from(jpeg()),
    );
    let wrong = router(state.clone()).oneshot(r).await.unwrap().status();
    assert_eq!(wrong, unknown);
}

#[tokio::test]
async fn delete_unknown_photo_matches_not_yours() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v["name"].as_str().unwrap().to_string();
    let hex = "c".repeat(32);
    let ghost = format!("quiet-river-calm-pond-1a2b_{hex}_ghost.jpg");
    let unknown = router(state.clone())
        .oneshot(request(
            "DELETE",
            &format!("/api/photos/{ghost}"),
            &[key_header(KEY_A)],
            Body::empty(),
        ))
        .await
        .unwrap()
        .status();
    assert_eq!(unknown, StatusCode::NOT_FOUND);
    let wrong = router(state.clone())
        .oneshot(request(
            "DELETE",
            &format!("/api/photos/{name}"),
            &[key_header(KEY_B)],
            Body::empty(),
        ))
        .await
        .unwrap()
        .status();
    assert_eq!(wrong, unknown);
}

#[tokio::test]
async fn wrong_key_on_private_photo_locks_out() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "mine.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v["name"].as_str().unwrap().to_string();
    let uri = format!("/photos/{name}");
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
    // No-key requests never burn the bucket (only supplied-but-wrong keys do),
    // so the owner's key still serves: lockout only stops guessing.
    let resp = router(state.clone())
        .oneshot(get(&uri, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn resolve_slug_verify_wrong_key_locks_out() {
    let (state, _tmp) = test_state().await;
    let (_, v) = resolve(&state, KEY_A).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    for _ in 0..10 {
        let r = post_json(
            "/api/galleries/resolve",
            &[key_header(KEY_B)],
            &format!(r#"{{"slug":{slug:?}}}"#),
        );
        let resp = router(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    let r = post_json(
        "/api/galleries/resolve",
        &[key_header(KEY_B)],
        &format!(r#"{{"slug":{slug:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // The real key still resolves: lockout only stops guessing.
    let r = post_json(
        "/api/galleries/resolve",
        &[key_header(KEY_A)],
        &format!(r#"{{"slug":{slug:?}}}"#),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn public_listing_with_incidental_key_burns_nothing() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug_a = v["slug"].as_str().unwrap().to_string();
    assert_eq!(publish(&state, &slug_a, true).await, StatusCode::OK);
    // KEY_B owns a different gallery: it is an incidental stored key here.
    let (_, _) = put_photo(&state, "b.jpg", Some(KEY_B), None, "image/jpeg", jpeg()).await;
    let uri = format!("/api/galleries/{slug_a}/photos");
    for _ in 0..11 {
        let resp = router(state.clone())
            .oneshot(get(&uri, &[key_header(KEY_B)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("cache-control").unwrap(),
            "private, no-store"
        );
    }
    // Gallery page HTML is revalidatable, never stale.
    let resp = router(state.clone())
        .oneshot(get(&format!("/{slug_a}"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
    // The bucket survived: lock the gallery and a wrong pass still 401s.
    let r = post_json(
        &format!("/api/galleries/{slug_a}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"s3cret-pass"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let resp = router(state.clone())
        .oneshot(get(&format!("{uri}?pass=nope"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// --- phase 1: probes, secure cookies, stateless media cookies ---

#[tokio::test]
async fn healthz_ok_reports_free_bytes() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/healthz", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["ok"], serde_json::Value::Bool(true));
    assert!(v["free_bytes"].as_u64().is_some(), "{v}");
}

#[tokio::test]
async fn healthz_503_when_min_free_unmet() {
    let (state, _tmp) = test_state_with(Limits {
        min_free: u64::MAX,
        ..Limits::defaults()
    })
    .await;
    let resp = router(state).oneshot(get("/healthz", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v = body_json(resp).await;
    assert_eq!(v["ok"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn metrics_loopback_lists_metric_names() {
    let (state, _tmp) = test_state().await;
    let resp = router(state).oneshot(get("/metrics", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = String::from_utf8(body_bytes(resp).await).unwrap();
    for name in [
        "gally_uptime_seconds",
        "gally_free_bytes",
        "gally_galleries",
        "gally_photos",
        "gally_bytes",
        "gally_requests_total",
        "gally_responses_5xx_total",
        "gally_uploads_total",
        "gally_upload_bytes_total",
    ] {
        assert!(text.contains(name), "missing {name} in\n{text}");
    }
}

#[tokio::test]
async fn secure_cookies_flag_appends_secure() {
    let (state, _tmp) = test_state_with(Limits {
        secure_cookies: true,
        ..Limits::defaults()
    })
    .await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let resp = router(state)
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("; Secure"), "{cookie}");

    // Default stays unchanged: no Secure on a plain-HTTP deployment.
    let (plain, _tmp2) = test_state().await;
    let (_, v) = put_photo(&plain, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let resp = router(plain)
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(!cookie.contains("Secure"), "{cookie}");
}

#[tokio::test]
async fn secure_cookies_flag_applies_to_admin_login() {
    let _env = lock_admin_env().await;
    // SAFETY: ADMIN_ENV_LOCK is held, so no other test touches ADMIN_PASS.
    unsafe {
        std::env::set_var("ADMIN_PASS", "secure-admin");
    }
    let (state, _tmp) = test_state_with(Limits {
        secure_cookies: true,
        ..Limits::defaults()
    })
    .await;
    let resp = router(state)
        .oneshot(post_json(
            "/api/admin/unlock",
            &[],
            r#"{"password":"secure-admin"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("; Secure"), "{cookie}");
    unsafe {
        std::env::remove_var("ADMIN_PASS");
    }
}

#[tokio::test]
async fn expired_media_token_is_rejected() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    // Correctly signed but expired: only the expiry check can reject it.
    let tok = media_token(&state.media_secret, &slug, MediaToken::Owner, 0, 1);
    let cookie = format!("galowner_{slug}={tok}");
    let resp = router(state)
        .oneshot(get(
            &format!("/photos/{name}"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn media_secret_file_is_private_and_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let first = AppState::new_with(tmp.path(), Limits::defaults())
        .await
        .expect("first state");
    let secret = first.media_secret.clone();
    assert!(!secret.is_empty());
    drop(first);

    let path = tmp.path().join(".gally-media-secret");
    let on_disk = std::fs::read_to_string(&path).expect("secret file");
    assert_eq!(on_disk.trim().len(), 64, "32 bytes hex-encoded");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only");
    }

    let second = AppState::new_with(tmp.path(), Limits::defaults())
        .await
        .expect("second state");
    assert_eq!(second.media_secret, secret, "secret must survive restarts");
}

// --- phase 2: DB listing, pagination, thumbnails ---

/// Insert a photo row's `created` directly so ordering is deterministic
/// (uploads in one test often share a millisecond).
fn set_created(state: &Arc<AppState>, name: &str, created: i64) {
    state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE photos SET created = ?1 WHERE name = ?2",
            rusqlite::params![created, name],
        )
        .unwrap();
}

#[tokio::test]
async fn listing_orders_newest_first() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    let (_, b) = put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    let name_b = b["name"].as_str().unwrap().to_string();
    let (_, c) = put_photo(
        &state,
        "c.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    let name_c = c["name"].as_str().unwrap().to_string();
    set_created(&state, &name_a, 100);
    set_created(&state, &name_b, 200);
    set_created(&state, &name_c, 300);

    let resp = router(state)
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?limit=10"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let names: Vec<&str> = v["photos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![name_c.as_str(), name_b.as_str(), name_a.as_str()]
    );
    assert_eq!(v["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn listing_paginates_without_gaps_or_duplicates() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    let name_a = a["name"].as_str().unwrap().to_string();
    let mut all = vec![name_a.clone()];
    for (file, created) in [("b.jpg", 200), ("c.jpg", 300)] {
        let (_, v) = put_photo(&state, file, Some(KEY_A), Some(&slug), "image/jpeg", jpeg()).await;
        let name = v["name"].as_str().unwrap().to_string();
        set_created(&state, &name, created);
        all.push(name);
    }
    set_created(&state, &name_a, 100);

    let base = format!("/api/galleries/{slug}/photos?limit=2");
    let resp = router(state.clone())
        .oneshot(get(&base, &[key_header(KEY_A)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let page1 = body_json(resp).await;
    let first: Vec<String> = page1["photos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(first.len(), 2);
    let cursor = page1["next"]
        .as_str()
        .expect("full page has a cursor")
        .to_string();
    assert_eq!(cursor, format!("200:{}", first[1]));

    let resp = router(state.clone())
        .oneshot(get(
            &format!("{base}&before={cursor}"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    let page2 = body_json(resp).await;
    let second: Vec<String> = page2["photos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(second.len(), 1);
    assert_eq!(page2["next"], serde_json::Value::Null);

    let mut seen = first;
    seen.extend(second);
    assert_eq!(seen.len(), all.len());
    seen.sort();
    all.sort();
    assert_eq!(seen, all, "every photo exactly once");
}

#[tokio::test]
async fn bad_listing_cursor_is_400() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let base = format!("/api/galleries/{slug}/photos");
    for bad in ["abc:name", "123", "1%3A"] {
        let resp = router(state.clone())
            .oneshot(get(&format!("{base}?before={bad}"), &[key_header(KEY_A)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn delete_photo_removes_listing_row() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let name = v["name"].as_str().unwrap().to_string();
    let r = request(
        "DELETE",
        &format!("/api/photos/{name}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM photos WHERE name = ?1",
            [&name],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn delete_gallery_removes_listing_rows() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.jpg", Some(KEY_A), None, "image/jpeg", jpeg()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    put_photo(
        &state,
        "b.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg(),
    )
    .await;
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM photos WHERE slug = ?1",
            [&slug],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn upload_creates_thumb_and_serves_it() {
    let (state, _tmp) = test_state().await;
    let (status, v) = put_photo(&state, "pic.png", Some(KEY_A), None, "image/png", png()).await;
    assert_eq!(status, StatusCode::CREATED);
    let name = v["name"].as_str().unwrap().to_string();
    let thumb = state.dir.join(".thumbs").join(format!("{name}.jpg"));
    assert!(thumb.is_file(), "thumbnail must exist after upload");

    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/thumb/{name}.jpg"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/jpeg");
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "private, no-store"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..2], &[0xFF, 0xD8], "JPEG magic");

    // Private gallery: no key, no cookie = 404.
    let resp = router(state.clone())
        .oneshot(get(&format!("/photos/thumb/{name}.jpg"), &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stale_thumb_without_photo_row_is_404() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.png", Some(KEY_A), None, "image/png", png()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let ghost = format!("{slug}_{}_ghost.jpg", "a".repeat(32));
    // A stray derived file for a name with no photos row (restored backup,
    // or a delete whose thumb removal was lost).
    let orphan = state.dir.join(".thumbs").join(format!("{ghost}.jpg"));
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, [0xFFu8, 0xD8, 0x00]).unwrap();
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/thumb/{ghost}.jpg"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(orphan.exists(), "only serving is refused");
}

#[tokio::test]
async fn thumb_accepts_viewer_cookie() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.png", Some(KEY_A), None, "image/png", png()).await;
    let slug = v["slug"].as_str().unwrap().to_string();
    let name = v["name"].as_str().unwrap().to_string();
    assert_eq!(publish(&state, &slug, true).await, StatusCode::OK);
    let r = post_json(
        &format!("/api/galleries/{slug}/lock"),
        &[key_header(KEY_A)],
        r#"{"password":"thumb-pass"}"#,
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos?pass=thumb-pass"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = set_cookie_pair(&resp, &format!("galpass_{slug}=")).expect("viewer cookie");
    let resp = router(state)
        .oneshot(get(
            &format!("/photos/thumb/{name}.jpg"),
            &[("cookie", cookie.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_or_bad_thumb_paths_are_404() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(
        &state,
        "note.txt",
        Some(KEY_A),
        None,
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    let name = v["name"].as_str().unwrap().to_string();
    // Unsupported source: no thumb was generated.
    let resp = router(state.clone())
        .oneshot(get(
            &format!("/photos/thumb/{name}.jpg"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    for uri in [
        "/photos/thumb/nope.jpg",
        "/photos/thumb/..%2F..%2Fgalleries.db.jpg",
        "/photos/thumb/",
    ] {
        let resp = router(state.clone())
            .oneshot(get(uri, &[key_header(KEY_A)]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn delete_photo_removes_thumb() {
    let (state, _tmp) = test_state().await;
    let (_, v) = put_photo(&state, "pic.png", Some(KEY_A), None, "image/png", png()).await;
    let name = v["name"].as_str().unwrap().to_string();
    let thumb = state.dir.join(".thumbs").join(format!("{name}.jpg"));
    assert!(thumb.is_file());
    let r = request(
        "DELETE",
        &format!("/api/photos/{name}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(!thumb.exists(), "thumb goes with its photo");
}

#[tokio::test]
async fn delete_gallery_removes_thumbs() {
    let (state, _tmp) = test_state().await;
    let (_, a) = put_photo(&state, "a.png", Some(KEY_A), None, "image/png", png()).await;
    let slug = a["slug"].as_str().unwrap().to_string();
    let (_, b) = put_photo(
        &state,
        "b.png",
        Some(KEY_A),
        Some(&slug),
        "image/png",
        png(),
    )
    .await;
    let thumbs: Vec<_> = [a["name"].as_str().unwrap(), b["name"].as_str().unwrap()]
        .iter()
        .map(|name| state.dir.join(".thumbs").join(format!("{name}.jpg")))
        .collect();
    for thumb in &thumbs {
        assert!(thumb.is_file(), "{}", thumb.display());
    }
    let r = request(
        "DELETE",
        &format!("/api/galleries/{slug}"),
        &[key_header(KEY_A)],
        Body::empty(),
    );
    assert_eq!(
        router(state.clone()).oneshot(r).await.unwrap().status(),
        StatusCode::OK
    );
    for thumb in &thumbs {
        assert!(!thumb.exists(), "{}", thumb.display());
    }
    let rows: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM photos WHERE slug = ?1",
            [&slug],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn exif_capture_date_orders_listing() {
    let (state, _tmp) = test_state().await;
    let slug = state
        .db
        .lock()
        .unwrap()
        .get_or_create_gallery(KEY_A)
        .expect("gallery");

    // Uploaded newer first: upload order would put "older" last.
    let (status, _) = put_photo(
        &state,
        "newer.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg_with_exif("2021:02:03 04:05:06"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = put_photo(
        &state,
        "older.jpg",
        Some(KEY_A),
        Some(&slug),
        "image/jpeg",
        jpeg_with_exif("2020:01:02 03:04:05"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let resp = router(state.clone())
        .oneshot(get(
            &format!("/api/galleries/{slug}/photos"),
            &[key_header(KEY_A)],
        ))
        .await
        .unwrap();
    let listed = body_json(resp).await;
    let photos = listed["photos"].as_array().unwrap();
    assert_eq!(photos.len(), 2);
    let newer = photos[0]["name"].as_str().unwrap();
    let older = photos[1]["name"].as_str().unwrap();
    assert!(newer.ends_with("_newer.jpg"), "{newer}");
    assert!(older.ends_with("_older.jpg"), "{older}");

    let db = state.db.lock().unwrap();
    let created = |name: &str| -> i64 {
        db.query_row(
            "SELECT created FROM photos WHERE slug = ?1 AND name = ?2",
            rusqlite::params![slug, name],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(created(newer), 1_612_325_106_000);
    assert_eq!(created(older), 1_577_934_245_000);
}

#[tokio::test]
async fn startup_backfills_exif_capture_date() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("photos")).unwrap();
    let mut db = gally_uring::db::open(&root.join("galleries.db")).expect("open db");
    let slug = db.get_or_create_gallery(KEY_A).expect("gallery");
    let name = format!("{slug}_{}_old.jpg", "e".repeat(32));
    std::fs::write(
        root.join("photos").join(&name),
        jpeg_with_exif("2020:01:02 03:04:05"),
    )
    .unwrap();
    assert_eq!(db.add_upload(&slug, &name, 1, 1), Some((1, 1)));
    drop(db);

    let state = AppState::new_with(root, Limits::defaults())
        .await
        .expect("restart");
    let db = state.db.lock().unwrap();
    let created: i64 = db
        .query_row(
            "SELECT created FROM photos WHERE slug = ?1 AND name = ?2",
            rusqlite::params![slug, name],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created, 1_577_934_245_000);
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
}
