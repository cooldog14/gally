FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN mkdir -p /data /backups \
    && chown 10001:10001 /data /backups \
    && chmod 700 /data /backups
COPY --from=build /src/target/release/gally-uring /usr/local/bin/gally-uring
USER 10001:10001
ENV HOST=0.0.0.0 PORT=8000 GALLY_ROOT=/data
EXPOSE 8000
STOPSIGNAL SIGTERM
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/gally-uring"]
