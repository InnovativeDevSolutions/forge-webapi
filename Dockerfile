# forge-api — multi-stage build.
# Stage 1 compiles the release binary; stage 2 ships a slim runtime image
# with only the binary and CA certificates.

FROM rust:1-bookworm AS builder

WORKDIR /app

# Copy manifests first so dependency compilation is cached between builds.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/forge-api /usr/local/bin/forge-api

# The API listens on 8080 (FORGE_API_BIND).
EXPOSE 8080

CMD ["forge-api"]
