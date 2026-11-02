# Gally (Rust backend)

Simple gallery to share your photos. Key-scoped galleries, no logins.

```sh
cargo run --release
```

Env: `PORT` (default 8000), `GALLY_ROOT` (data dir, default crate root),
`GALLY_RATE_MAX` (uploads per IP per minute, default 60, 0 = unlimited),
`GALLY_INVITE_CODE` (when set, new galleries require this code via the
`X-Invite-Code` header; unset = open creation; contributing to an existing
gallery never needs it), `ADMIN_PASS` (enables `/api/admin/*`), `RUST_LOG`
(e.g. `gally_uring=debug` for per-request and per-stage upload/listing timings).

Run tests: `cargo test` (unit + API), `cargo test --release --test stress`
(live-server stress suite, needs a release build first).

Each gallery page is one freeform canvas: `GET|PUT /api/galleries/{slug}/canvas`
(JSON `{"blocks":[...]}` with image/text/audio blocks and absolute positions;
writes need the owner `X-Gallery-Key`). Media is uploaded from the same page and
served under `/photos/`; owners edit the canvas in place, anyone with the link
sees it rendered.

Galleries that never got content (no media files, no canvas blocks) are
deleted 5 minutes after creation, on a 10-minute timer: unused links do not
accumulate. The key is not banned, so it can mint a new gallery.
