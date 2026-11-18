# Gally

Gally is a small, self-hosted gallery for sharing photos. A gallery is
identified by a key you choose, not an account: anyone with the key can
upload, edit, publish, protect, or delete that gallery. New galleries are
**private by default**; the owner can publish one from edit mode. There are no
accounts, email, tracking, or key/password recovery.

- `cargo run --release` gives you a local demo.
- The supported deployment is `compose.yaml`: one non-root image plus a daily
  `backup` sidecar.
- One node only: local disk plus SQLite. See
  [Scaling ceiling](#scaling-ceiling).

## Quickstart (local)

```sh
cargo run --release
```

Open <http://127.0.0.1:8000>. The first dialog asks for a key:

- Existing key: 6–128 ASCII characters, no spaces or accents.
- New key: at least 12 characters, with a capital letter and a symbol (for
  example `Sunset-Beach-42`).

Save the key; it is the only way to edit that gallery. New galleries are
private until the owner ticks **Public** in edit mode. `ADMIN_PASS` and
`GALLY_INVITE_CODE` are unset by default, so admin and the invite gate are off.

The app writes `galleries.db` and `photos/` under the working directory; set
`GALLY_ROOT=/path` to move them. The local run uses plain HTTP with secure
cookies off. Do not expose it directly.

## Configuration

All values are read at startup; restart the process or container to apply
them.

| Variable | Default | Description |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address; keep loopback behind a local proxy. The image sets `0.0.0.0` because it binds inside a container. |
| `PORT` | `8000` | Listen port. |
| `GALLY_ROOT` | working directory | Data dir: `galleries.db`, `photos/`, generated media secret. The image uses `/data`. |
| `GALLY_RATE_MAX` | `60` | Uploads per IP per minute; `0` = unlimited. Failed key/password attempts stay capped at 10 per minute per IP. |
| `GALLY_READ_RATE_MAX` | `12000` | Listings plus photo reads per IP per minute; `0` = unlimited. Keep it above `GALLY_GALLERY_MAX_PHOTOS`: one gallery view costs one read per photo. |
| `GALLY_MAX_BYTES` | `10737418240` (10 GiB) | Single upload cap. |
| `GALLY_GALLERY_MAX_BYTES` | `5368709120` (5 GiB) | Per-gallery byte cap; `0` = off. |
| `GALLY_GALLERY_MAX_PHOTOS` | `5000` | Photos per gallery; `0` = off. |
| `GALLY_MIN_FREE_BYTES` | `2147483648` (2 GiB) | Free space kept on the data volume; `0` = off. |
| `GALLY_UPLOAD_SLOTS` | `32` | Concurrent uploads overall; minimum 1. |
| `GALLY_UPLOAD_SLOTS_PER_IP` | `4` | Concurrent uploads per IP; minimum 1. |
| `GALLY_ARGON2_SLOTS` | `4` | Concurrent Argon2 password hashes; minimum 1. |
| `GALLY_INVITE_CODE` | unset | When set, new galleries need this code via `X-Invite-Code`; existing galleries never do. |
| `ADMIN_PASS` | unset | Enables `/api/admin/*` and the admin login. When unset, admin login returns 404. |
| `GALLY_TRUST_PROXY` | off (`127.0.0.0/8,172.16.0.0/12` in Compose) | Comma-separated IPs/CIDRs of every reverse-proxy hop allowed to set `X-Forwarded-For`; `1/true/yes` means loopback. Never enable it for a directly reachable app. |
| `GALLY_SECURE_COOKIES` | off (`1` in Compose) | `1/true/yes`: add `Secure` to cookies; requires HTTPS. Set `0` for plain-HTTP LAN use. |
| `GALLY_MEDIA_SECRET` | generated | Optional media-cookie signing secret; unset = generated once into `<GALLY_ROOT>/.gally-media-secret`. |
| `RUST_LOG` | `gally_uring=info` | Log filter, e.g. `gally_uring=debug` for per-request timings. |

## Admin

Set `ADMIN_PASS` to a strong value and restart; then open
<http://127.0.0.1:8000/gally/admin/>. With `ADMIN_PASS` unset, the page loads
but the login returns 404.

The admin can:

- list galleries (slug, photo count, bytes, visibility);
- delete one gallery and its files;
- sweep files whose gallery row is gone, plus expired upload temp files.

The admin cannot recover a key or a viewer password, and cannot read a private
gallery's photos. Deleting a gallery does not revoke its key: the same key
still works for a future upload. The admin session lasts 24 hours; login is
rate-limited and locks out after repeated failures. Use **Log out** on a shared
machine.

`GALLY_INVITE_CODE` is separate: it gates creation of new galleries, not admin
access. Existing galleries never need the invite.

## Production (single node, Docker)

`compose.yaml` runs the server and a `backup` sidecar from the same image, both
as UID/GID `10001`. CI publishes the image to GHCR on every push to `master`
(`latest` and `sha-<commit>`) and on `v*` tags (the tag name):

```sh
# .env beside compose.yaml, chmod 600
GALLY_IMAGE=ghcr.io/cooldog14/gally:latest
ADMIN_PASS=...
GALLY_INVITE_CODE=...
GALLY_MEDIA_SECRET=...
GALLY_SECURE_COOKIES=1
```

```sh
docker compose pull
docker compose up -d --no-build
docker compose ps
docker compose logs -f gally
```

The package is private by default. Either make it public in the package
settings, or log in first with a token that can read packages:

```sh
docker login ghcr.io -u cooldog14
```

To build on the host instead of pulling:

```sh
docker compose up -d --build
```

`GALLY_IMAGE` is Compose-only; its default is the local tag
`gally-uring:local`. Pin a `sha-<commit>` or `v*` tag for reproducible
deploys; `latest` moves.

The app listens on `0.0.0.0:8000` inside the container and publishes to
`127.0.0.1:8000` on the host. Terminate TLS in a reverse proxy:

- Host proxy (nginx/Caddy on this host): upstream `http://127.0.0.1:8000`.
  The default `GALLY_TRUST_PROXY` covers the Docker bridge peer.
- Proxy container: attach it to the `gally` network and use
  `http://gally:8000`; its bridge address is trusted by default.
- Proxy on another host: set `GALLY_BIND=0.0.0.0` and narrow
  `GALLY_TRUST_PROXY` to the proxy IP/CIDR, then firewall port 8000 to that
  host.

`GALLY_TRUST_PROXY` lists every reverse-proxy hop. The app walks
`X-Forwarded-For` right-to-left and accepts the first address outside that
list; a malformed chain falls back to the connection peer. Never extend it to
a directly reachable client range. For two hops (an edge proxy forwarding to a
local Caddy) list the edge proxy's address too, or every client resolves to
the edge and shares its rate limits.

`docker compose stop` sends SIGTERM and waits up to `stop_grace_period: 120s`
for in-flight requests; longer uploads are cut. Startup deletes partial
uploads, so an interrupted upload cannot resume across a restart and starts
that file from zero. `docker compose down` does the same.

The named volumes (`gally-data`, `gally-backups`) inherit the image's
ownership. A host bind mount does not: `chown -R 10001:10001 ./data` (and
`./backups`) first, or the app cannot create its lock, secret, and database.
Keep `.env` `0600`; it holds secrets.

Server-side previews are generated at upload time for new
JPG/PNG/GIF/WebP/BMP/ICO photos. Photos uploaded before this version have no
preview and the grid falls back to the original; there is no backfill command.

## Health and metrics

- `GET /healthz` returns 200 when SQLite answers, the photos directory is
  readable, and free space is at least `GALLY_MIN_FREE_BYTES`; otherwise 503
  with `{"ok":false,...}`. Use it for proxy and container checks.
- `GET /metrics` exposes Prometheus text metrics and is local-only: a direct
  connection from a trusted peer (the Docker gateway for a host scrape) or a
  forwarded client that resolves to loopback. Scrape
  `http://127.0.0.1:8000/metrics` on the host. Forwarded public requests get
  404, so the proxy never exposes it.

## Backup and restore

The `backup` sidecar runs the same image as `--backup`: it snapshots
immediately when it starts, then every 24 hours into `/backups/current`
(the `gally-backups` volume). A failed run exits and the container restarts;
watch its restart count. There is no separate timer to install.

The snapshot copies `photos/` first and then snapshots the database with
`VACUUM INTO`, so a failed photo copy leaves the previous snapshot's database
intact; prune failures fail the run. In-flight `.part-` files and thumbnail
temp files are skipped, while the derived `.thumbs/` previews are copied so a
restore keeps them. The destination is `0700`, the database copy `0600`, and
it is overwritten in place: deleted top-level photos are pruned, but stale
previews inside `.thumbs/` may remain in the snapshot and are never served. A
destination inside the live data dir is refused.

**Retention:** `current` is a single snapshot; the app keeps no history. Add
retention in the off-host tool, for example `restic forget --keep-daily 7
--keep-weekly 4` after its backup.

**Off-host copy is not configured.** Copy `gally-backups` into rclone/restic
from the host and alert on failures, for example:

```sh
docker compose stop backup
mkdir -p extract
docker compose run --rm -T --no-deps --entrypoint tar backup \
  -C /backups/current -cf - . | tar -xf - -C extract
docker compose start backup
```

**Restore drill** (rehearse it on a copy before you need it):

```sh
docker compose stop gally backup
mkdir -p extract
docker compose run --rm -T --no-deps --entrypoint tar backup \
  -C /backups/current -cf - . | tar -xf - -C extract
docker compose run --rm -T --no-deps --entrypoint sh backup \
  -c 'find /data -mindepth 1 -delete'
tar -C extract -cf - . | docker compose run --rm -T --no-deps \
  --entrypoint tar backup -C /data -xf -
docker compose start gally backup
curl -fsS http://127.0.0.1:8000/healthz
```

Setting `GALLY_MEDIA_SECRET` in `.env` keeps media cookies valid across
restores; the generated `<GALLY_ROOT>/.gally-media-secret` is not in the
snapshot. Then open a gallery and confirm the listing and photos load. On
startup the server recounts photo counts and byte totals, rebuilds the listing
table from the photos directory, and sweeps stray `.part`/`.tmp-*` temp files;
this repairs a restored database whose counters or index disagree with the
files.

## Monitoring

There is no built-in alerting or dashboard. Watch at least:

- Disk free on the host filesystem backing the Docker volumes, alerted well
  above `GALLY_MIN_FREE_BYTES` (the app only refuses writes at the floor; it
  does not page anyone).
- Restart counts and state for both containers (`docker compose ps`,
  `docker inspect -f '{{.RestartCount}}'`).
- The `backup` container logs: success prints the snapshot path, failure exits
  and retries. `docker compose logs backup`.
- 5xx rate from `docker compose logs gally` (and your proxy's access log).
- `/healthz` from an external checker and `/metrics` from a local Prometheus
  scrape, if you run one.

## Scaling ceiling

One node only: state is local disk plus SQLite (`galleries.db`) plus photos,
and a startup flock on `<GALLY_ROOT>/.gally.lock` prevents a second instance.
Per-IP rate limits are in process memory. Stateless signed media cookies
survive process restarts and allow any future instance sharing the same
signing secret; DB-backed paginated listings remove the per-view
whole-directory scan. They still do not make the app multi-node.

Multi-node would need S3-compatible object storage for photos, Postgres or
another shared database, and rate limiting at the edge. **That is not
implemented.** Scale vertically instead: bigger disk, more upload slots.

## Development and CI

Run tests: `cargo test` (unit + API + client smoke when `node` is installed).
The live-server suite is opt-in and needs a release build:

```sh
cargo build --release
cargo test --release --features stress --test stress
```

CI (`.github/workflows/ci.yml`) runs on push and pull requests on
`ubuntu-latest`: `cargo fmt --check`, then
`cargo clippy --all-targets -- -D warnings`, then `cargo test` and
`cargo build --release`. A separate job builds the Docker image, validates
`compose.yaml`, and smoke-tests the container's `/healthz`. Pushes to `master`
and `v*` tags publish the image to GHCR. The stress suite is not run in CI.

Photos are ordered by EXIF `DateTimeOriginal` when present, falling back to
upload time; existing libraries are backfilled once on the first startup after
the upgrade. The browser uploads in 8 MiB chunks and resumes from the server's
committed offset after a failure; other clients can keep sending the whole
body in one `PUT`.

HTTP contract: [`docs/API.md`](docs/API.md). Binding contributor rules:
[`AGENTS.md`](AGENTS.md).

## License

MIT. See [`LICENSE`](LICENSE).
