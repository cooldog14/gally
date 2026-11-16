//! Runs `tests/client_smoke.js` when node exists.
//! The script drives app.js in a stub DOM; its three user workflows (upload,
//! edit, view) also talk to a real server booted here, so client and server
//! contracts are tested together. Two shipping bugs (`KEY_MIN_LEN`, the lock
//! probe) were invisible to the Rust suite, hence the client checks.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    port: u16,
    _tmp: tempfile::TempDir,
}

impl Server {
    fn boot() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("free port")
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_gally-uring"))
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("GALLY_ROOT", tmp.path())
            .env("GALLY_RATE_MAX", "0")
            .env("GALLY_READ_RATE_MAX", "0")
            .env("GALLY_MIN_FREE_BYTES", "0")
            .env("RUST_LOG", "error")
            .env_remove("ADMIN_PASS")
            .env_remove("GALLY_INVITE_CODE")
            .current_dir(tmp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "server never came up on {port}");
            std::thread::sleep(Duration::from_millis(25));
        }
        Self {
            child,
            port,
            _tmp: tmp,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn client_smoke() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not installed, skipping client smoke test");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("tests/client_smoke.js");
    let server = Server::boot();
    let out = Command::new("node")
        .arg(&script)
        .current_dir(root)
        .env(
            "GALLY_BASE_URL",
            format!("http://127.0.0.1:{}", server.port),
        )
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "client smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
