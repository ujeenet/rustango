# The deployable image. `Dockerfile.dev` is the hot-reload one
# docker-compose.yml uses; this one is for shipping.
#
#   docker build -t platform_commerce_saas:latest .
#   docker run --rm -p 8080:8080 -e DATABASE_URL=... platform_commerce_saas:latest
#
# Pick a non-default backend at build time:
#   docker build --build-arg FEATURES=sqlite --build-arg NO_DEFAULT=1 .

FROM rust:1-bookworm AS builder
ARG FEATURES=""
ARG NO_DEFAULT=""
WORKDIR /src
COPY . .
# Incremental compilation only grows the cache in a one-shot image build.
ENV CARGO_INCREMENTAL=0
RUN set -eux; \
    flags="--release --locked"; \
    if [ -n "$NO_DEFAULT" ]; then flags="$flags --no-default-features"; fi; \
    if [ -n "$FEATURES" ]; then flags="$flags --features $FEATURES"; fi; \
    cargo build $flags; \
    mkdir -p /out; \
    cp target/release/platform_commerce_saas /out/

# Same Debian release as the builder: the binary links the builder's glibc.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/platform_commerce_saas /usr/local/bin/platform_commerce_saas
# Migrations and settings are read at run time, so they travel with the
# binary rather than being baked into it.
COPY migrations /app/migrations
COPY config /app/config
# Never root. The numeric id keeps the ownership stable if the image is
# rebuilt on a host whose useradd picks a different one.
RUN useradd --uid 10001 --create-home app && chown -R 10001 /app
USER 10001
WORKDIR /app
EXPOSE 8080
# `.with_health()` mounts /health; drop this line if you removed it.
HEALTHCHECK --interval=5s --timeout=3s --start-period=20s --retries=12 \
  CMD curl -fsS http://127.0.0.1:8080/health || exit 1
CMD ["platform_commerce_saas"]
