# Gally (Rust backend)

Simple gallery to share your photos. Key-scoped galleries, no logins.

```sh
cargo run --release
```

Env: `PORT` (default 8000), `GALLY_ROOT` (data dir, default crate root),
`GALLY_RATE_MAX` (uploads per IP per minute, default 60, 0 = unlimited),
`ADMIN_PASS` (enables `/api/admin/*`), `RUST_LOG` (e.g. `gally_uring=debug`
for per-request and per-stage upload/listing timings).

Run tests: `cargo test` (unit + API), `cargo test --release --test stress`
(live-server stress suite, needs a release build first).
