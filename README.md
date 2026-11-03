# Gally (Rust backend)

Simple gallery to share your photos. Key-scoped galleries, no logins. The key
is typed by hand: at least 6 characters. A key you *create* in the UI asks for
12+ characters with a capital letter and a symbol (e.g. `Sunset-Beach-42`).
New galleries are **private by default**: the uploader's key (or the media
cookie issued to it) is the only way to see them until the owner ticks
`Public` in edit mode. Galleries created before this behaviour stay public.

```sh
cargo run --release
```

Env: `PORT` (default 8000), `GALLY_ROOT` (data dir, default the working
directory),
`GALLY_RATE_MAX` (uploads per IP per minute, default 60, 0 = unlimited),
`GALLY_READ_RATE_MAX` (listings + photo reads per IP per minute, default 12000,
0 = unlimited; one gallery view spends one request per photo, so keep it above
`GALLY_GALLERY_MAX_PHOTOS`), `GALLY_MAX_BYTES` (single upload, default 10 GiB),
`GALLY_GALLERY_MAX_BYTES` (per gallery, default 5 GiB, 0 = off),
`GALLY_GALLERY_MAX_PHOTOS` (per gallery, default 5000, 0 = off),
`GALLY_MIN_FREE_BYTES` (free space always kept on the data volume, default
2 GiB, 0 = off), `GALLY_UPLOAD_SLOTS` / `GALLY_UPLOAD_SLOTS_PER_IP` / `GALLY_ARGON2_SLOTS` (concurrent
uploads total / per IP / password hashes, default 32 / 4 / 4, minimum 1),
`GALLY_INVITE_CODE` (when set, new galleries require this code via the
`X-Invite-Code` header; unset = open creation; contributing to an existing
gallery never needs it), `ADMIN_PASS` (enables `/api/admin/*`),
`GALLY_TRUST_PROXY` (when `1/true/yes`, use the rightmost `X-Forwarded-For`
entry as the client IP, but only when the connection itself is loopback;
needed so per-IP limits see real clients behind a local reverse proxy), `RUST_LOG`
(e.g. `gally_uring=debug` for per-request and per-stage upload/listing timings).

## Backup

```sh
GALLY_ROOT=/var/gally ./gally-uring --backup /var/backups/gally-2026-10-30
```

`VACUUM INTO` snapshots the database consistently while the server runs and the
photos are copied next to it (no in-flight `.part-` files). The destination is
created `0700`; the database copy is `0600`. A destination inside the live data
dir is refused; re-running into the same directory overwrites the database only
on success and prunes photos deleted from the source.

Run tests: `cargo test` (unit + API + client smoke when `node` is installed);
the live-server suite is opt-in and needs a release build:
`cargo build --release && cargo test --release --features stress --test stress`.
