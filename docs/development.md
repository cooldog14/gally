# Development

The server is a Rust binary. The client is static files with no build step.

## Tests

```sh
cargo test
```

Runs unit, API, and client smoke tests (client smoke needs `node`). The
live-server stress suite is opt-in and needs a release build:

```sh
cargo build --release
cargo test --release --features stress --test stress
```

## Format and lint

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## CI

`.github/workflows/ci.yml` runs on push, pull requests, and manual dispatch on
`ubuntu-latest`. The test job runs `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, and
`cargo build --release`. After that job passes, the Docker job builds the image,
validates `compose.yaml`, and smoke-tests the container's `/healthz`.
Only pushes and manual runs for `master` or `v*` tags publish to GHCR, and
only after those image checks pass. They publish a `sha-<short-commit>` tag
plus `latest` for `master` or the matching version tag for a tag ref.
The stress suite is not run in CI.

## Image checks

```sh
docker build -t gally-uring:ci .
docker compose config --quiet
```

See [deployment.md](deployment.md) for running the deployment.
