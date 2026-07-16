# Two-stage build from source. The binary is self-contained (UI assets are
# compiled in); the runtime image needs nothing but certificates for guests'
# outbound https.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p keel-engine

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/keel /usr/local/bin/keel
# All state lives in one directory: mount a volume here.
WORKDIR /var/lib/keel
VOLUME /var/lib/keel
EXPOSE 8080
ENTRYPOINT ["keel"]
CMD ["serve", "--db", "/var/lib/keel/keel.db", "--listen", "0.0.0.0:8080"]
