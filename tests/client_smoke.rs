//! Runs `tests/client_smoke.js` (stub-DOM check of app.js) when node exists:
//! the client has no other automated check, and two shipping bugs (`KEY_MIN_LEN`,
//! the lock probe) were invisible to the Rust suite.

#[test]
fn client_smoke() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("tests/client_smoke.js");
    let out = match std::process::Command::new("node")
        .arg(&script)
        .current_dir(root)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("node not installed, skipping client smoke test");
            return;
        }
        Err(e) => panic!("run {}: {e}", script.display()),
    };
    assert!(
        out.status.success(),
        "client smoke failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
