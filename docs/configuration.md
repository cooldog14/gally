# Configuration

All values are read at startup; restart the process or container to apply them.

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

`GALLY_IMAGE` and `GALLY_BIND` are Compose-only (image tag and host bind
address), not app settings. See [deployment.md](deployment.md).
