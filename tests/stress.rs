//! Live-server stress suite: spawns the real binary, hammers it.
//! Run: cargo test --release --test stress -- --nocapture --test-threads=1
//! Each test boots its own server on a unique port with a temp root.
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Server {
    child: Child,
    port: u16,
    _tmp: tempfile::TempDir,
}

impl Server {
    fn boot(admin_pass: Option<&str>) -> Self {
        Self::boot_with_env(admin_pass, &[])
    }

    fn boot_with_env(admin_pass: Option<&str>, env: &[(&str, &str)]) -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let port = free_port();
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/release/gally-uring");
        assert!(bin.exists(), "build release first: {}", bin.display());
        let mut cmd = Command::new(bin);
        cmd.env("PORT", port.to_string())
            .env("GALLY_ROOT", tmp.path())
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(p) = admin_pass {
            cmd.env("ADMIN_PASS", p);
        } else {
            cmd.env_remove("ADMIN_PASS");
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn server");
        // wait for readiness
        let url = format!("http://127.0.0.1:{port}/app.js");
        let t0 = Instant::now();
        loop {
            if t0.elapsed() > Duration::from_secs(10) {
                panic!("server on {port} never came up");
            }
            if let Ok(r) = reqwest::blocking::get(&url) {
                if r.status() == StatusCode::OK {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port, _tmp: tmp }
    }

    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .build()
        .unwrap()
}

async fn put(client: &Client, srv: &Server, key: &str, name: &str, bytes: &[u8]) -> reqwest::Response {
    client
        .put(srv.url(&format!("/api/photos/{name}")))
        .header("X-Gallery-Key", key)
        .header("Content-Type", "image/jpeg")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_uploads_same_key() {
    let srv = Server::boot_with_env(None, &[("GALLY_RATE_MAX", "0")]);
    let client = client();
    let body = vec![0xABu8; 200_000];
    let t0 = Instant::now();
    let mut handles = vec![];
    for t in 0..32usize {
        let (c, b) = (client.clone(), body.clone());
        let url = srv.url("/api/photos/");
        handles.push(tokio::spawn(async move {
            let mut ok = 0;
            for i in 0..25 {
                let r = c
                    .put(format!("{url}t{t}-{i}.jpg"))
                    .header("X-Gallery-Key", "stresskey1")
                    .header("Content-Type", "image/jpeg")
                    .body(b.clone())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::CREATED, "t{t} i{i}: {}", r.text().await.unwrap());
                ok += 1;
            }
            ok
        }));
    }
    let mut total = 0;
    for h in handles {
        total += h.await.unwrap();
    }
    let el = t0.elapsed();
    println!("800 PUTs (200KB) in {el:.1?} = {:.0}/s", 800.0 / el.as_secs_f64());
    assert_eq!(total, 800);
    // listing must contain all 800
    let slug: String = client
        .post(srv.url("/api/galleries/resolve"))
        .json(&serde_json::json!({"key": "stresskey1"}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["slug"]
        .as_str()
        .unwrap()
        .into();
    let list: serde_json::Value = client
        .get(srv.url(&format!("/api/galleries/{slug}/photos")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["photos"].as_array().unwrap().len(), 800);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_downloads_one_file() {
    let srv = Server::boot(None);
    let client = client();
    let body = vec![0xCDu8; 1_000_000];
    let r = put(&client, &srv, "dlkey1", "big.jpg", &body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let name: String = r.json::<serde_json::Value>().await.unwrap()["name"].as_str().unwrap().into();
    let t0 = Instant::now();
    let mut handles = vec![];
    for _ in 0..64usize {
        let (c, u) = (client.clone(), srv.url(&format!("/photos/{name}")));
        handles.push(tokio::spawn(async move {
            let mut oks = 0;
            for _ in 0..10 {
                let b = c.get(&u).send().await.unwrap().bytes().await.unwrap();
                assert_eq!(b.len(), 1_000_000);
                assert!(b.iter().all(|v| *v == 0xCD));
                oks += 1;
            }
            oks
        }));
    }
    let mut total = 0;
    for h in handles {
        total += h.await.unwrap();
    }
    let el = t0.elapsed();
    println!("640 GETs (1MB) in {el:.1?} = {:.0} MB/s", 640.0 / el.as_secs_f64());
    assert_eq!(total, 640);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_read_write_churn() {
    let srv = Server::boot_with_env(Some("s3cret"), &[("GALLY_RATE_MAX", "0")]);
    let client = client();
    // unlock admin
    let r = client
        .post(srv.url("/api/admin/unlock"))
        .json(&serde_json::json!({"password": "s3cret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let cookie = r.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    // writers
    let mut handles = vec![];
    for t in 0..8usize {
        let (c, u) = (client.clone(), srv.url("/api/photos/"));
        handles.push(tokio::spawn(async move {
            for i in 0..20 {
                let r = c
                    .put(format!("{u}w{t}-{i}.png"))
                    .header("X-Gallery-Key", "churnkey")
                    .header("Content-Type", "image/png")
                    .body(vec![7u8; 50_000])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::CREATED);
            }
        }));
    }
    // readers hammer listing + statics while writes land
    for _ in 0..4usize {
        let (c, u1, u2) = (client.clone(), srv.url("/app.js"), srv.url("/"));
        handles.push(tokio::spawn(async move {
            for _ in 0..40 {
                assert_eq!(c.get(&u1).send().await.unwrap().status(), StatusCode::OK);
                assert_eq!(c.get(&u2).send().await.unwrap().status(), StatusCode::OK);
            }
        }));
    }
    // deleter removes what writers create (best effort; 404 ok)
    {
        let (c, ck) = (client.clone(), cookie.clone());
        let list_url = srv.url("/api/galleries/resolve");
        handles.push(tokio::spawn(async move {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let slug: String = c
                    .post(&list_url)
                    .json(&serde_json::json!({"key": "churnkey"}))
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()["slug"]
                    .as_str()
                    .unwrap()
                    .into();
                let l: serde_json::Value = c
                    .get(srv.url(&format!("/api/galleries/{slug}/photos")))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if let Some(n) = l["photos"].as_array().unwrap().first() {
                    let nm = n["name"].as_str().unwrap();
                    let st = c
                        .delete(srv.url(&format!("/api/photos/{nm}")))
                        .header("Cookie", ck.clone())
                        .send()
                        .await
                        .unwrap()
                        .status();
                    assert!(st == StatusCode::OK || st == StatusCode::NOT_FOUND);
                }
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    println!("mixed churn done, no deadlock/panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edge_cases() {
    let srv = Server::boot(None);
    let client = client();
    // empty body
    let r = client
        .put(srv.url("/api/photos/e.jpg"))
        .header("X-Gallery-Key", "edgekey1")
        .header("Content-Type", "image/jpeg")
        .body(vec![])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // 21 MiB over cap → 413
    let r = client
        .put(srv.url("/api/photos/big.jpg"))
        .header("X-Gallery-Key", "edgekey1")
        .header("Content-Type", "image/jpeg")
        .body(vec![0u8; 21 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE, "got {}", r.status());
    // dotfile + traversal
    for p in ["/photos/.hidden", "/photos/a%2Fb", "/photos/%2e%2e/server.py"] {
        let r = client.get(srv.url(p)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{p}");
    }
    // unknown gallery slug shape vs missing
    let r = client.get(srv.url("/api/galleries/nope-1/photos")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = client.get(srv.url("/no-such-gallery-1")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // delete nonexistent
    let r = client
        .delete(srv.url("/api/photos/ghost-x_00000000000000000000000000000000_y.jpg"))
        .header("X-Gallery-Key", "edgekey1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // rate limit: 61 rapid PUTs, last must be 429
    let mut last = StatusCode::OK;
    for i in 0..61 {
        let r = client
            .put(srv.url(&format!("/api/photos/rl{i}.gif")))
            .header("X-Gallery-Key", "ratelimitkey")
            .header("Content-Type", "image/gif")
            .body(vec![1u8; 100])
            .send()
            .await
            .unwrap();
        last = r.status();
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS, "rate limiter never tripped");
    println!("edge cases all correct");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_directory_degradation() {
    let srv = Server::boot_with_env(None, &[("GALLY_RATE_MAX", "0")]);
    let client = client();
    // seed 2000 files directly (bypass HTTP for speed)
    let dir = srv._tmp.path().join("photos");
    for i in 0..2000 {
        tokio::fs::write(dir.join(format!("seed_{i:05}.jpg")), [1u8; 100]).await.unwrap();
    }
    let t0 = std::time::Instant::now();
    let r = put(&client, &srv, "bigdir1", "one.jpg", &[2u8; 1000]).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let slug: String = r.json::<serde_json::Value>().await.unwrap()["slug"].as_str().unwrap().into();
    // listing with 2001 files must still respond quickly (<2s) and contain ours
    let t1 = std::time::Instant::now();
    let list: serde_json::Value = client
        .get(srv.url(&format!("/api/galleries/{slug}/photos")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dt = t1.elapsed();
    assert!(list["photos"].as_array().unwrap().iter().any(|p| p["name"].as_str().unwrap().ends_with("one.jpg")));
    println!("2000-file listing in {dt:.1?} (total setup+up {t0:?})");
    assert!(dt < Duration::from_secs(2), "listing too slow: {dt:?}");
}
