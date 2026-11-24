# Configuration

All values are read at startup; restart the process or container to apply them.

| Variable | Default | Description |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address; keep loopback behind a local proxy. The image sets `0.0.0.0` because it binds inside a container. |
| `PORT` | `8000` | Listen port. |
| `GALLY_ROOT` | working directory | Data dir: `galleries.db`, `photos/`, generated media secret. The image uses `/data`. |
| `GALLY_RATE_MAX` | `60` | Upload bucket hits per IP per minute; `0` = unlimited. On throttle, `Retry-After` gives the seconds to wait. The separate key/password failure cap stays at 10 per minute per IP. |
| `GALLY_CHUNK_RATE_MAX` | `600` | Resumable upload chunks per IP per minute; `0` = unlimited. Every chunk counts, including the first, which also consumes the upload bucket. |
| `GALLY_READ_RATE_MAX` | `12000` | Listings plus photo reads per IP per minute; `0` = unlimited. Keep it above `GALLY_GALLERY_MAX_PHOTOS`: one gallery view costs one read per photo. |
| `GALLY_MAX_BYTES` | `10737418240` (10 GiB) | Single upload cap. |
| `GALLY_GALLERY_MAX_BYTES` | `5368709120` (5 GiB) | Per-gallery byte cap; `0` = off. |
| `GALLY_GALLERY_MAX_PHOTOS` | `5000` | Photos per gallery; `0` = off. |
| `GALLY_MIN_FREE_BYTES` | `2147483648` (2 GiB) | Free space kept on the data volume; `0` = off. |
| `GALLY_UPLOAD_SLOTS` | `32` | Concurrent uploads overall; minimum 1. |
| `GALLY_UPLOAD_SLOTS_PER_IP` | `4` | Concurrent uploads per IP; minimum 1. |
| `GALLY_ARGON2_SLOTS` | `4` | Concurrent Argon2 password hashes; minimum 1. |
| `GALLY_INVITE_CODE` | unset | When set, new galleries need this code via `X-Invite-Code`; existing galleries never do. Wrong codes count toward the 10/min/IP failure bucket; once full, further attempts are `429`. A correct code still works. |
| `ADMIN_PASS` | unset | Enables `/api/admin/*` and the admin login. When unset, admin login returns 404. |
| `GALLY_TRUST_PROXY` | off, including Compose | List the exact IP address of each actual reverse-proxy hop allowed to set `X-Forwarded-For` (CIDRs are supported; `1/true/yes` trusts loopback). Do not trust an entire Docker subnet. Leave off for a directly reachable app. |
| `GALLY_SECURE_COOKIES` | off (`1` in Compose) | `1/true/yes`: add `Secure` to cookies; requires HTTPS. Set `0` for plain-HTTP LAN use. |
| `GALLY_MEDIA_SECRET` | generated | Optional media-cookie signing secret; must be at least 32 bytes, shorter values fail startup. Unset = 32 random bytes generated once into `<GALLY_ROOT>/.gally-media-secret` (`0600`), hex-encoded. Rotating it invalidates all media cookies. |
| `RUST_LOG` | `gally_uring=info` | Log filter, e.g. `gally_uring=debug` for per-request timings. |

### Data directory

The app leaves `GALLY_ROOT` itself as-is; it does not chmod it. `photos/` is
`0700`; `galleries.db` (including `-wal`/`-shm`) and `.gally-media-secret` are
`0600`. At startup it warns if the root is group/other-accessible or not owned
by the app user. Use a dedicated directory owned by the app user — never
`/tmp`, `/`, or a shared web root.

`GALLY_IMAGE` and `GALLY_BIND` are Compose-only (image tag and host bind
address), not app settings. See [deployment.md](deployment.md).

`/metrics` only accepts a direct loopback socket from an untrusted peer with
no forwarding headers. In Compose, scrape from the app container's network
namespace. Block metrics and admin routes on the public reverse proxy.
