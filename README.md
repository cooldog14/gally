# Gally

Small, self-hosted gallery for sharing photos. A gallery is identified by a key
you choose, not an account: anyone with the key can upload, edit, publish,
protect, or delete that gallery. The URL carries a generated public slug; the
key is the secret you type. New galleries are **private by default**. There are
no accounts, email, tracking, or key/password recovery.

- `cargo run --release` gives you a local demo.
- The supported deployment is `compose.yaml`: one non-root image plus a daily
  `backup` sidecar.
- One node only: local disk plus SQLite.

## Quickstart (local)

```sh
cargo run --release
```

Open <http://127.0.0.1:8000> and enter a key:

- Existing key: 6–128 ASCII characters, no spaces or accents.
- New key: at least 12 characters, with a capital letter and a symbol (for
  example `Sunset-Beach-42`).

The four-word string in the URL after the first upload (for example
`/brave-vole-juniper-badger-88e9`) is the generated gallery slug, not the key.
The key banner shows what you typed; save it, because it is the only way to
edit that gallery. New galleries stay private until the owner ticks **Public**
in edit mode. `ADMIN_PASS` and `GALLY_INVITE_CODE` are unset by default, so
admin and the invite gate are off.

The app writes `galleries.db` and `photos/` under the working directory; set
`GALLY_ROOT=/path` to move them. The local run is plain HTTP with secure
cookies off; do not expose it directly.

Uploads are stored as sent; previews and EXIF capture times are derived. Photos
are ordered by EXIF `DateTimeOriginal` when present, falling back to upload
time; existing libraries are backfilled once on the first startup after the
upgrade. The browser uploads in 8 MiB chunks and resumes from the server's
committed offset after a failure; other clients can send the whole body in one
`PUT`.

## Docs

| Doc | Covers |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | Environment variables |
| [`docs/deployment.md`](docs/deployment.md) | Docker, TLS, admin, health, backup, restore, monitoring, scaling |
| [`docs/API.md`](docs/API.md) | HTTP contract |
| [`docs/development.md`](docs/development.md) | Build, tests, CI |

## License

MIT. See [`LICENSE`](LICENSE).
