# AGENTS.md

Binding rules for AI agents changing Gally. If a rule here is wrong, change
this file first (with the reason), then the code.

## North star

Gally is a small, self-hosted gallery for sharing photos. The gallery key is
the identity: there are no accounts, email, or recovery.

## Product philosophy

- A key owns a gallery. Anyone with the key can upload, edit, publish,
  protect, or delete that gallery.
- New galleries are private by default. Public is an opt-in action by the
  owner; a viewer password is a second, separate gate.
- The admin can list, delete, and sweep galleries. The admin cannot recover a
  key or password and cannot read a private gallery's photos.
- Losing the owner key means the gallery can no longer be edited or deleted by
  its owner. Say this plainly; never promise recovery.
- One node. SQLite, local disk, in-process rate limits, no distributed state.
- Uploads are stored as sent. Previews and EXIF times are derived; originals
  are never rewritten.
- No runtime tracking or third-party network calls. The browser loads
  same-origin assets; the server calls no external services.
- Boring, small, direct. Prefer the existing helper/pattern over a new
  abstraction. There is no frontend build step.
- Fail closed on auth; fail safe on data. On an error, keep files, return 5xx,
  and let startup recount repair the index. Never delete user data to make an
  error go away.
- Ops are deliberately unmagical: TLS is the reverse proxy's job, off-host
  backup is the operator's job, and the app ships one local snapshot plus
  health/metrics. The data-dir lock enforces one writer.

## Non-goals

Do not build:

- accounts, logins, email, password reset, or key recovery;
- albums, folders, tags, or search;
- multi-node, S3, Postgres, Redis, or external queues;
- image/video editing or transcoding;
- feeds, discovery, likes, comments, analytics, or tracking;
- a frontend framework or build step;
- third-party CDNs or runtime network calls;
- built-in TLS termination (use a reverse proxy);
- built-in off-host backup or retention (the sidecar writes one local
  snapshot; the operator copies it off-host).

## MUST / MUST NOT

### Dependencies

- **MUST NOT** add a runtime dependency without a written reason in the
  change. Prefer std/platform.
- The client **MUST** stay build-free and **MUST NOT** add third-party
  runtime JS/CSS or fetch assets from a CDN. The existing vendored HEIC
  decoder is the one allowed third-party file; keep new ones out.

Do: a 30-line use of `std` instead of a crate.
Don't: add a date/time or CLI crate for one call.

### Data safety

- **MUST** preserve user data on errors.
- **MUST NOT** delete a file to mask a DB error.
- **MUST** keep schema changes additive and migratable; **MUST NOT** drop user
  data.

Do: keep the renamed file, return 500, and let recount repair the row.
Don't: remove the file because `add_upload` failed.

### Errors

- **MUST NOT** panic on malformed input. No `unwrap`/`expect` on
  user-controlled body, header, path, DB, or file results in request paths.
  Mutex locks and test setup are exceptions.
- Handlers return the existing JSON error shape (`{"error":"..."}`) with a
  meaningful status code.

Do: `match`, `?` into `ApiError`, or return a JSON error.
Don't: `serde_json::from_slice(...).unwrap()` in a handler.

### Security

- **MUST NOT** weaken auth, rate limits, or cookie security to make a test
  pass.
- **MUST** use the existing credential checks: hash gallery keys with
  `key_hash`, verify viewer passwords with Argon2id, compare invite/admin
  secrets in constant time, and verify signed tokens by their HMAC. Never add
  a direct `==` on a secret.
- **MUST** keep keys, passwords, and cookies out of logs.
- **MUST** keep admin actions and `/metrics` off public surfaces.
- **MUST** use `textContent` for user-controlled text in the client; never
  inject it with `innerHTML`.

Do: add a test that proves the denial path.
Don't: set `GALLY_SECURE_COOKIES=0` in a test that claims to model TLS.

### Originals

- **MUST** store uploads as sent.
- Previews and EXIF times are derived; **MUST NOT** re-encode or strip the
  stored original.

Do: write `.thumbs/<stored-name>.jpg`; read EXIF for ordering.
Don't: transcode, resize, or rewrite the file under `photos/`.

### Client

- **MUST** keep the gallery client a single `app.js` with no build step.
  `admin.html` is the existing standalone admin page.
- **MUST** use platform APIs and same-origin assets.
- **MUST** keep inline scripts in `index.html` to the existing theme bootstrap;
  gallery behavior goes in `app.js`.

Do: load the local HEIC decoder only when a HEIC actually needs decoding.
Don't: add a `<script src="https://...">` or a package manager.

### Tests

- **MUST** include a test that fails without the behavior change.
- **MUST** run the verification commands below before calling the change done.
- API tests **MUST** boot the real server; client smoke tests **MUST** drive
  `app.js` against the real server.
- **MUST NOT** mock the DB or assert only that a function was called.
- Stress tests stay opt-in behind `--features stress`.

Do: add an API test for a new status code; add a client-smoke step for a new
dialog flow.
Don't: test wiring, echo the source, or pin incidental defaults.

### UI copy

- **MUST** use sentence case everywhere, including buttons.
- Errors **MUST** state the cause and the fix. No exclamation or marketing.

Do: `Password required.` / `Upload failed: file is too large (10 GB max).`
Don't: `Oops! Something went wrong.`

### Docs

- **MUST** update `docs/configuration.md` for a new or changed environment
  variable, `docs/deployment.md` for an operator-visible change, and
  `docs/API.md` for an HTTP contract change.
- **MUST** update `README.md` when the entry point, quickstart, or doc index
  changes.
- **MUST** keep this file current when a rule changes.
- Commits **SHOULD** have a clear imperative subject and a body explaining
  why; conventional prefixes (`fix:`, `feat:`, `ci:`) are optional.

## Style

### Rust

- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are the floor.
- `anyhow` at startup/CLI boundaries; `ApiError` for handler errors.
- Never block the async runtime: `spawn_blocking` for Argon2, image decode,
  DB open/recount and other CPU/disk work.
- DB access stays synchronous behind `Mutex<Connection>`; do not add an async
  DB layer or ORM.
- Keep routes in `src/routes.rs` and schema/migrations in `src/db.rs` until a
  split clearly removes code.
- No abstraction for one caller; delete dead code instead of leaving a shim.
- Use `tracing`, not `println!`, outside the CLI usage/error path.

### JavaScript and CSS

- `app.js` is one file: `"use strict"`, 2-space indent, semicolons, `const`
  by default, `let` only when reassigned, `async`/`await`, native DOM APIs.
- No framework, no build, no client-side router. The server serves the SPA.
- `styles.css` is one file; theme via the `data-theme` attribute and the
  existing CSS variables. No preprocessor.
- Keep dialogs native (`<dialog>`), labels linked, and live errors in
  `role="alert"`/`role="status"` elements.

### Tests

- Unit tests live beside the module; API tests in `tests/api.rs`; client
  smoke in `tests/client_smoke.js` via `tests/client_smoke.rs`.
- Use temporary directories and ephemeral ports; never depend on a fixed
  port or the developer's data dir.
- Prefer one focused assertion set per behavior over a broad integration
  test that hides the failure.

## Verification

Run from the repo root:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

For the live-server stress suite:

```sh
cargo build --release
cargo test --release --features stress --test stress
```

For the image path:

```sh
docker build -t gally-uring:ci .
docker compose config --quiet
```

## When in doubt

- If a change alters privacy, authentication, data safety, or the single-node
  assumption, stop and update this file and the operator docs before coding.
- If a rule blocks a correct fix, change the rule in the same change and say
  why. Do not quietly violate it.
