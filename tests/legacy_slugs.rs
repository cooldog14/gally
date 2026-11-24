//! Regression: pre-aee47b7 galleries (`happy-otter-7`) must survive the
//! startup recount and work through listing, media and delete.
//!
//! This uses the true old schema (no `photos` table, no visibility columns),
//! unlike the migration tests whose slugs are already in the current format.

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use gally_uring::{AppState, router, state::Limits};
use tower::ServiceExt;

const KEY: &str = "Old-Key-123";
const SLUG: &str = "happy-otter-7";

fn sock() -> SocketAddr {
    "127.0.0.1:4242".parse().unwrap()
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut r = b.body(Body::empty()).unwrap();
    r.extensions_mut().insert(ConnectInfo(sock()));
    r
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn legacy_slug_listing_media_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let photos = root.join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    let name = format!("{SLUG}_{}_old.jpg", "a".repeat(32));
    let bytes = b"legacy photo bytes".to_vec();
    std::fs::write(photos.join(&name), &bytes).unwrap();
    {
        let conn = rusqlite::Connection::open(root.join("galleries.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL UNIQUE, created INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO galleries(slug, key_hash, created) VALUES(?1, ?2, 1)",
            rusqlite::params![SLUG, gally_uring::db::key_hash(KEY)],
        )
        .unwrap();
    }

    let state = AppState::new_with(root, Limits::defaults())
        .await
        .expect("start on an old-format gallery");

    // Startup recount must keep the legacy photo, so the public listing has it.
    let r = request("GET", &format!("/api/galleries/{SLUG}/photos"), &[]);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listing = body_json(resp).await;
    assert!(
        listing["photos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == name),
        "recount keeps the legacy photo row: {listing}"
    );

    let r = request("GET", &format!("/photos/{name}"), &[]);
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let served = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(served.as_ref(), bytes.as_slice());

    let rows: Vec<(String, i64)> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT name, size FROM photos WHERE slug = ?1")
            .unwrap();
        stmt.query_map([SLUG], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(rows, vec![(name.clone(), bytes.len() as i64)]);

    let r = request(
        "DELETE",
        &format!("/api/galleries/{SLUG}"),
        &[("x-gallery-key", KEY)],
    );
    let resp = router(state.clone()).oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !photos.join(&name).exists(),
        "delete sweeps the legacy photo"
    );
}
