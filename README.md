# Gally

Small, self-hosted gallery for sharing photos. A gallery is identified by its
URL; the owner key proves ownership of that gallery. Anyone with the key and
the URL can upload, edit, publish, protect, or delete it. The URL carries a
generated public slug; the key is the secret you type. Using the same key
again starts a new gallery, never opens an existing one. New galleries are
**private by default**. There are no accounts, email, tracking, or key/password
recovery.

- `cargo run --release` gives you a local demo.
- The supported deployment is `compose.yaml`: one non-root image plus a daily
  `backup` sidecar.
- One node only: local disk plus SQLite.

## Quickstart (local)

```sh
cargo run --release
```

Open <http://127.0.0.1:8000> and click **Upload**. The first upload asks for a
new gallery key: at least 12 characters, with a capital letter and a symbol
(for example `Sunset-Beach-42`). Save it; it is the only way to edit that
gallery.

A key is not a gallery name. Every upload from the home page starts a **new**
gallery with a new URL, even when you reuse the same key; a key alone never
opens another gallery. To open an existing gallery, use its link; enter its
key (6–128 ASCII characters, no spaces or accents) when prompted. The
four-word string in the URL after the first upload (for example
`/brave-vole-juniper-badger-88e9`) is the generated gallery slug, not the key.
Galleries created before 2026-10-30 have shorter `adj-noun-N` slugs (for
example `/happy-otter-7`); those old links and files keep working, while new
galleries get the four-word form.
The key banner has **Copy key**; edit mode has **Show key** if you dismissed
it. New galleries stay private until the owner ticks **Public** in edit mode;
a viewer password only gates a public gallery. `ADMIN_PASS` and
`GALLY_INVITE_CODE` are unset by default, so admin and the invite gate are
off.

The app writes `galleries.db` and `photos/` under the working directory; set
`GALLY_ROOT=/path` to move them. The local run is plain HTTP with secure
cookies off; do not expose it directly.

Uploads are stored as sent; previews and EXIF capture times are derived. Photos
are ordered by EXIF `DateTimeOriginal` when present, falling back to upload
time. Files above 16 MiB skip EXIF extraction and still upload unchanged with
that fallback; only the timestamp read is skipped, never the upload. Existing
libraries are backfilled once on the first startup after the upgrade. The
browser uploads in 8 MiB chunks and resumes from the server's committed offset
after a failure; other clients can send the whole body in one `PUT`.

## Docs

| Doc | Covers |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | Environment variables |
| [`docs/deployment.md`](docs/deployment.md) | Docker, TLS, admin, health, backup, restore, monitoring, scaling |
| [`docs/API.md`](docs/API.md) | HTTP contract |
| [`docs/development.md`](docs/development.md) | Build, tests, CI |

## License

MIT. See [`LICENSE`](LICENSE).
