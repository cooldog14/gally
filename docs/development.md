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

`.github/workflows/ci.yml` runs on push and pull requests on `ubuntu-latest`:
`cargo fmt --check`, then `cargo clippy --all-targets -- -D warnings`, then
`cargo test` and `cargo build --release`. A separate job builds the Docker
image, validates `compose.yaml`, and smoke-tests the container's `/healthz`.
Pushes to `master` and `v*` tags publish the image to GHCR. The stress suite is
not run in CI.

## Image checks

```sh
docker build -t gally-uring:ci .
docker compose config --quiet
```

See [deployment.md](deployment.md) for running the deployment.
