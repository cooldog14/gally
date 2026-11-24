# Deployment

Single node only: local disk plus SQLite. Environment variables are documented
in [configuration.md](configuration.md); the HTTP contract is in
[API.md](API.md).

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

`GALLY_MEDIA_SECRET` must be at least 32 bytes; shorter values fail startup.
Rotating it invalidates all media cookies.

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
- Proxy container: attach it to the `gally` network and use
  `http://gally:8000`.
- Proxy on another host: set `GALLY_BIND=0.0.0.0` and set
  `GALLY_TRUST_PROXY` to the exact proxy IP, then firewall port 8000 to that
  host.

Forwarded IPs are not trusted by default, including on Docker. Set
`GALLY_TRUST_PROXY` to the exact peer IP(s) of your proxy hops as seen by the
app (for a host proxy this can be the Docker gateway's individual address).
Use stable addresses and do not trust a whole Docker bridge or any client
range. The app walks `X-Forwarded-For` right-to-left and accepts the first
address outside that list; a malformed chain falls back to the connection
peer. Configure your proxies to set the chain, and list every trusted hop
if there are multiple. Without this, clients can share the proxy's rate
limits.

At the public reverse proxy, block `/metrics`, `/gally/admin` and
`/gally/admin/` (including subpaths), and `/api/admin` and its subpaths.
Keep authenticated admin access on a separate private path to the service,
such as the loopback-published port over an SSH tunnel.

When upgrading from a version that allowed public media to be cached, purge
all cached originals and previews under `/photos/` from any CDN or caching
proxy. New response headers cannot invalidate copies already cached by public
clients, and previously downloaded files cannot be recalled.

`docker compose stop` sends SIGTERM and waits up to `stop_grace_period: 120s`
for in-flight requests; longer uploads are cut. Startup deletes partial
uploads, so an interrupted upload cannot resume across a restart and starts
that file from zero. `docker compose down` does the same.

The named volumes (`gally-data`, `gally-backups`) inherit the image's
ownership. A host bind mount does not: `chown -R 10001:10001 ./data` (and
`./backups`) first, or the app cannot create its lock, secret, and database.
The app leaves the root's own mode as-is; `photos/` is `0700`, and
`galleries.db` (with `-wal`/`-shm`) and `.gally-media-secret` are `0600`. It
warns if the root is group/other-accessible or not owned by the app user. Use
a dedicated directory — never `/tmp`, `/`, or a shared web root. Keep `.env`
`0600`; it holds secrets.

Server-side previews are generated at upload time for new
JPG/PNG/GIF/WebP/BMP/ICO photos. Photos uploaded before this version have no
preview and the grid falls back to the original; there is no backfill command.

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
access. Existing galleries never need the invite. Wrong codes count toward
the 10/min/IP failure bucket; the first attempts are `403`, and further
attempts are `429` once the bucket is full. A correct code still works.

## Health and metrics

- `GET /healthz` returns 200 when the `galleries` and `photos` tables are
  readable, the photos directory is readable, and free space is at least
  `GALLY_MIN_FREE_BYTES`; otherwise 503 with `{"ok":false,...}`. Use it for
  proxy and container checks. Expired upload temps are reaped by a background
  sweep every ten minutes (and opportunistically by `/healthz`), so upload
  cleanup does not depend on an external probe.
- `GET /metrics` exposes Prometheus text only to an actual loopback socket
  peer that is not a trusted proxy, without `X-Forwarded-For`. A forwarded
  address does not make a scrape local. On Docker, scrape from the app's
  network namespace; the published host port is not a loopback peer as seen
  by the app. For a local check on the Linux Docker host:

  ```sh
  pid=$(docker inspect -f '{{.State.Pid}}' "$(docker compose ps -q gally)")
  sudo nsenter --target "$pid" --net curl --noproxy '*' -fsS http://127.0.0.1:8000/metrics
  ```

## Backup and restore

The `backup` sidecar runs the same image as `--backup`: it snapshots
immediately when it starts, then every 24 hours into `/backups/current`
(the `gally-backups` volume). A failed run exits and the container restarts;
watch its restart count. There is no separate timer to install.

The sidecar takes a SQLite `VACUUM INTO` snapshot, then copies its indexed
originals and their available `.thumbs/` previews into a private sibling
directory (`0700`, database `0600`). Unindexed files and upload/thumbnail
temps are not copied. An original already gone when copied is dropped from
the snapshot's index and counts; other I/O errors fail the run. If the live
index has photo rows but none of their files are present, the run fails and
leaves the previous snapshot in place rather than publishing an empty one.
Changes to the live gallery after the DB snapshot are not guaranteed to be
included.

The same rule protects startup: if the database has photo rows but `photos/`
contains none of them, the server refuses to rebuild an empty index and
exits. Restore the files, or move both `galleries.db` and `photos/` aside to
start a fresh library.

After syncing the files and directories, the sidecar publishes the complete
directory as `current` with an atomic rename. Replacing an existing snapshot
requires Linux/filesystem support for `renameat2(RENAME_EXCHANGE)`, as on
ordinary Linux Docker local volumes. Without it the previous snapshot stays
in place and the command fails; there is no non-atomic fallback. Budget disk
space for both versions during the copy. A failed pre-publication run does
not change the installed snapshot. After publication is synced, removing the
previous version is best-effort; an interrupted run can leave a private
`.gally-stage-*` sibling. Inspect any such directory with the sidecar stopped
before deciding whether it is safe to remove.

For manual `--backup DEST`, use a dedicated backup location with no nested
mounts, under a parent you control. `DEST` must be missing, empty, or an
existing Gally snapshot (`galleries.db` and `photos/` only). Destination
symlinks and overlaps with the live data or photo tree are refused.
If an older directory has extra entries, leave it intact rather than
deleting files to make it pass validation. Stop the sidecar, inspect and
archive the old directory, and use `--backup` to write a fresh unused
sibling. After verifying that sibling, point the sidecar and off-host copy
at the new path explicitly.

**Retention:** `current` is a single snapshot; the app keeps no history. Add
retention in the off-host tool, for example `restic forget --keep-daily 7
--keep-weekly 4` after its backup.

**Off-host copy is not configured.** Save an archive somewhere private, then
copy it to your off-host storage with rclone/restic and alert on failures.
Stop only the sidecar while reading `current`, so the archive cannot span
two different snapshots:

```sh
umask 077
work=$(mktemp -d)
archive_status=0
docker compose stop backup
docker compose run --rm -T --no-deps --entrypoint tar backup \
  -C /backups/current -cf - galleries.db photos > "$work/gally.tar" || archive_status=$?
docker compose start backup
test "$archive_status" -eq 0
# Copy "$work/gally.tar" off-host only after this test passed.
```

**Restore drill:** Fetch a known-good archive you created into a fresh,
private working directory. Never extract an untrusted archive or restore over
an existing data volume. The commands below run in a shell from the Compose
directory. Set `archive` to the absolute path of the downloaded archive.

```sh
set -e
umask 077
: "${archive:?Set archive to the absolute path of the backup archive}"
test -f "$archive"
work=$(mktemp -d)
restore_volume="gally-restore-${work##*.}"
image=$(docker compose config --images | sort -u)  # server and sidecar use one image
if docker volume inspect "$restore_volume" >/dev/null 2>&1; then
  echo "Restore volume already exists; choose a fresh name" >&2
  exit 1
fi
docker volume create "$restore_volume"
docker run --rm -i --network none --read-only --user 10001:10001 \
  --mount "type=volume,src=$restore_volume,dst=/data" \
  --entrypoint /bin/sh "$image" -ec '
    umask 077
    entries=$(ls -A /data); test -z "$entries"
    tar -x -C /data --no-same-owner --no-same-permissions
    test -f /data/galleries.db
    test -s /data/galleries.db
    test -d /data/photos
    types=$(find /data -mindepth 1 ! -type f ! -type d -print -quit); test -z "$types"
    links=$(find /data -type f -links +1 -print -quit); test -z "$links"
    extras=$(find /data -mindepth 1 -maxdepth 1 ! -name galleries.db ! -name photos -print -quit); test -z "$extras"
  ' < "$archive"

override="compose.$restore_volume.yaml"
(set -C; cat > "$override" <<EOF
volumes:
  data:
    external: true
    name: $restore_volume
EOF
)
drill=$(docker compose -f compose.yaml -f "$override" run --rm -d --no-deps \
  -e GALLY_SECURE_COOKIES=0 -p 127.0.0.1::8000 gally)
docker port "$drill" 8000/tcp
```

`GALLY_SECURE_COOKIES=0` is only for this isolated HTTP drill. Never use it in
production or for a TLS test. Only the fresh volume is writable during
extraction, and the temporary app uses it without changing live routing or
running the sidecar. Open the printed
loopback address in a browser. Use a known gallery URL from the archive and
its owner key or viewer password if required. Confirm the expected listing
and open or download an **original**, not just a preview. Compare the bytes
or checksum with a known copy if available. A 200 from `/healthz` alone says
nothing about the recovered photos. If verification fails, stop the temporary
container and leave the active deployment untouched.

After a successful check, stop the temporary container before switching (only
one writer can open the restored volume). Keep the generated override beside
`compose.yaml` and include it in subsequent commands:

```sh
docker stop "$drill"
docker compose stop gally backup
docker compose -f compose.yaml -f "$override" up -d --no-build gally backup
```

Both services now use the recovered data volume. Before the sidecar is allowed
to replace it, keep an off-host copy of the currently installed snapshot.
Retain the former volume and the off-host archive. To roll back, stop both
services using the override,
then run `docker compose up -d --no-build gally backup` without it (or use
your prior override). New writes to the recovered volume will not be on the
former volume. Do not use `down -v` or delete either volume during the drill
or cutover.

Setting `GALLY_MEDIA_SECRET` in `.env` keeps media-cookie signing stable
across restores; it must be at least 32 bytes, and rotating it invalidates all
media cookies. The generated `<GALLY_ROOT>/.gally-media-secret` is 32 random
bytes (`0600`) and is not in the snapshot. Startup recounts photo counts and
byte totals, rebuilds the listing from the restored photos directory, and
sweeps upload/thumbnail temps.

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
