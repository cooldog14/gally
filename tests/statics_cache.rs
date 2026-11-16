use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
    response::Response,
};
use gally_uring::{AppState, router};
use tower::ServiceExt;

async fn state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = AppState::new(tmp.path()).await.expect("AppState::new");
    (state, tmp)
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut r = b.body(Body::empty()).unwrap();
    r.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:4242".parse::<SocketAddr>().unwrap()));
    r
}

fn header(resp: &Response, name: header::HeaderName) -> &str {
    resp.headers()
        .get(&name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

fn cache_control(resp: &Response) -> &str {
    header(resp, header::CACHE_CONTROL)
}

#[tokio::test]
async fn unversioned_asset_revalidates() {
    let (state, _tmp) = state().await;

    let resp = router(state.clone())
        .oneshot(get("/app.js", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(cache_control(&resp), "no-cache");
    assert!(!cache_control(&resp).contains("immutable"));
    let etag = header(&resp, header::ETAG).to_owned();
    assert!(!etag.is_empty());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!body.is_empty());

    let resp = router(state.clone())
        .oneshot(get("/app.js", &[("if-none-match", &etag)]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header(&resp, header::ETAG), etag);
    assert_eq!(cache_control(&resp), "no-cache");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());

    let resp = router(state.clone())
        .oneshot(get("/app.js", &[("if-none-match", "\"wrong\"")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn index_routes_revalidate() {
    let (state, _tmp) = state().await;

    for uri in ["/", "/index.html"] {
        let resp = router(state.clone()).oneshot(get(uri, &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(cache_control(&resp), "no-cache", "{uri}");
        assert!(!header(&resp, header::ETAG).is_empty(), "{uri}");
    }
}
