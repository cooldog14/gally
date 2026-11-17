# Gally (Rust backend)

Simple gallery to share your photos. Key-scoped galleries, no logins. The key
is typed by hand: at least 6 characters. A key you *create* in the UI asks for
12+ characters with a capital letter and a symbol (e.g. `Sunset-Beach-42`).
New galleries are **private by default**: the uploader's key (or the media
cookie issued to it) is the only way to see them until the owner ticks
`Public` in edit mode; the owner can also delete the gallery there. Galleries
that already exist keep their current visibility.

```sh
cargo run --release
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address; keep loopback behind a local proxy. The image sets `0.0.0.0` because it binds inside a container. |
| `PORT` | `8000` | Listen port. |
| `GALLY_ROOT` | working directory | Data dir: `galleries.db`, `photos/`, generated media secret. The image uses `/data`. |
| `GALLY_RATE_MAX` | `60` | Uploads per IP per minute; `0` = unlimited. |
| `GALLY_READ_RATE_MAX` | `12000` | Listings + photo reads per IP per minute; `0` = unlimited. Keep it above `GALLY_GALLERY_MAX_PHOTOS`: one gallery view costs one read per photo. |
| `GALLY_MAX_BYTES` | `10737418240` (10 GiB) | Single upload cap. |
| `GALLY_GALLERY_MAX_BYTES` | `5368709120` (5 GiB) | Per-gallery byte cap; `0` = off. |
| `GALLY_GALLERY_MAX_PHOTOS` | `5000` | Photos per gallery; `0` = off. |
| `GALLY_MIN_FREE_BYTES` | `2147483648` (2 GiB) | Free space kept on the data volume; `0` = off. |
| `GALLY_UPLOAD_SLOTS` | `32` | Concurrent uploads overall; minimum 1. |
| `GALLY_UPLOAD_SLOTS_PER_IP` | `4` | Concurrent uploads per IP; minimum 1. |
| `GALLY_ARGON2_SLOTS` | `4` | Concurrent Argon2 password hashes; minimum 1. |
| `GALLY_INVITE_CODE` | unset | When set, new galleries need this code via `X-Invite-Code`; existing galleries never do. |
| `ADMIN_PASS` | unset | Enables `/api/admin/*`. |
| `GALLY_TRUST_PROXY` | off (`127.0.0.0/8,172.16.0.0/12` in Compose) | Comma-separated IPs/CIDRs of every reverse-proxy hop allowed to set `X-Forwarded-For`; `1/true/yes` means loopback. Read once at startup. Never enable it for a directly reachable app. |
| `GALLY_SECURE_COOKIES` | off (`1` in Compose) | `1/true/yes`: add `Secure` to cookies; requires HTTPS. Set `0` for plain-HTTP LAN use. |
| `GALLY_MEDIA_SECRET` | generated | Optional media-cookie signing secret; unset = generated and kept in the data dir. |
| `RUST_LOG` | `gally_uring=info` | Log filter, e.g. `gally_uring=debug` for per-request timings. |

## Production (single node, Docker)

The supported deployment is `compose.yaml`: one non-root image, the `gally`
server and a `backup` sidecar built from the same image.

```sh
docker compose up -d --build
docker compose ps
docker compose logs -f gally
```

The app listens on `0.0.0.0:8000` inside the container and publishes to
`127.0.0.1:8000` on the host. Terminate TLS in your reverse proxy:

- Host proxy (nginx/Caddy on this host): upstream `http://127.0.0.1:8000`.
  The default `GALLY_TRUST_PROXY` covers the Docker bridge peer.
- Proxy container: attach it to the `gally` network and use
  `http://gally:8000`; its bridge address is trusted by default.
- Proxy on another host: set `GALLY_BIND=0.0.0.0` and narrow
  `GALLY_TRUST_PROXY` to the proxy IP/CIDR, then firewall port 8000 to that host.

`GALLY_TRUST_PROXY` lists every reverse-proxy hop. The app walks
`X-Forwarded-For` right-to-left and accepts the first address outside that
list; a malformed chain falls back to the connection peer. It defaults to
`127.0.0.0/8,172.16.0.0/12`; never extend it to a directly reachable client
range. `1/true/yes` still means loopback. For two hops (an edge proxy
forwarding to a local Caddy) list the edge proxy's address too, or every
client resolves to the edge and shares its rate limits.

Put secrets and other knobs in `.env` beside `compose.yaml` (compose also
interpolates it):

```sh
ADMIN_PASS=...
GALLY_INVITE_CODE=...
GALLY_MEDIA_SECRET=...
GALLY_SECURE_COOKIES=1
```

`GALLY_SECURE_COOKIES` defaults to `1` in `compose.yaml`; set it to `0` only
when the app is served over plain HTTP on a LAN.

`docker compose stop` sends SIGTERM and waits up to `stop_grace_period: 120s`
for in-flight requests; longer ones (a slow large upload) are cut. Startup
deletes partial uploads, so an interrupted upload cannot resume across a
restart and starts that file from zero. `docker compose down` does the same.

Both services run as UID/GID `10001`, and the image creates `/data` and
`/backups` with that ownership. The named volumes (`gally-data`,
`gally-backups`) inherit it. A host bind mount does not: `chown -R
10001:10001 ./data` (and `./backups`) first, or the app cannot create its
lock, secret and database. Keep `.env` `0600`; it holds secrets.

Server-side previews are generated at upload time for new JPG/PNG/GIF/WebP/BMP/ICO
photos; photos uploaded before this version have no preview and the grid falls
back to the original (no backfill command).

## Health and metrics

- `GET /healthz` returns 200 when SQLite answers, the photos directory is
  readable and free space is at least `GALLY_MIN_FREE_BYTES`; otherwise 503
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
intact; prune failures fail the run. In-flight `.part` files and thumbnail temp
files are skipped, while the derived `.thumbs/` previews are copied so a
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
`cargo build --release && cargo test --release --features stress --test stress`.

CI (`.github/workflows/ci.yml`) runs on push and pull requests on
`ubuntu-latest`: `cargo fmt --check`, then
`cargo clippy --all-targets -- -D warnings`, then `cargo test` and
`cargo build --release`. It also builds the Docker image, validates
`compose.yaml`, and smoke-tests the container's `/healthz`. The stress suite
is not run in CI.

Photos are ordered by EXIF `DateTimeOriginal` when present, falling back to
upload time; existing libraries are backfilled once on the first startup after
the upgrade. The browser uploads in 8 MiB chunks and resumes from the server's
committed offset after a failure; other clients can keep sending the whole
body in one `PUT`.
