# syntax=docker/dockerfile:1.7
#
# One Dockerfile, four images. Build context is the REPO ROOT, because
# both examples depend on `crates/rustango` by path:
#
#   docker build -f docker/soak/Dockerfile \
#     --build-arg APP=platform_commerce \
#     --build-arg FEATURES=postgres,mysql,sqlite \
#     -t rustango-soak/commerce:all .
#
# ## Why four and not six
#
# `platform_commerce` (single-tenant) compiles with all three backend
# features at once and picks the dialect at run time from
# `DATABASE_URL`'s scheme — so one image covers the tri-dialect matrix.
#
# `platform_commerce_saas` cannot. `DefaultTenantDb` resolves to
# `sqlx::Postgres` whenever the `postgres` feature is on, so a tenancy
# build's *registry* dialect is fixed at compile time. Hence one SaaS
# image per dialect, each built with exactly one backend feature.
#
# ## Why not the generated Dockerfile
#
# The examples ship their own (from the scaffolder), and it is the right
# one for a single-dialect deploy. This one exists because the soak
# needs the repo root as context and an `APP` switch; keeping it here
# rather than editing theirs keeps each example copy-pasteable.

FROM public.ecr.aws/docker/library/rust:1.88-bookworm AS builder
ARG APP=platform_commerce
ARG FEATURES=postgres,mysql,sqlite
WORKDIR /src
COPY . .
WORKDIR /src/crates/rustango/examples/${APP}
ENV CARGO_INCREMENTAL=0
# One BuildKit cache shared by all four builds — the same trade
# bin/examples.sh makes, for the same reason: each example's own target
# is ~1.3 GB, and four separate ones do not fit. `sharing=locked`
# serialises concurrent builds rather than corrupting the cache.
#
# The binaries must be copied out inside this same RUN: a cache mount
# does not survive into the next layer.
RUN --mount=type=cache,id=rustango-soak-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rustango-soak-target,target=/build,sharing=locked \
    CARGO_TARGET_DIR=/build \
    cargo build --locked --release \
      --no-default-features --features "${FEATURES}" \
 && mkdir -p /out \
 && cp /build/release/${APP} /out/app \
 && cp /build/release/commerce_worker /out/worker

# Same Debian release as the builder: the binaries link its glibc, and a
# mismatch fails at container start rather than at build.
FROM public.ecr.aws/docker/library/debian:bookworm-slim
ARG APP=platform_commerce
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/app    /usr/local/bin/app
COPY --from=builder /out/worker /usr/local/bin/worker
COPY crates/rustango/examples/${APP}/config     /app/config
COPY crates/rustango/examples/${APP}/migrations /app/migrations
# /data holds SQLite files on a named volume; /app/system holds the
# tenancy migrations the SaaS app generates.
RUN useradd --uid 10001 --create-home app \
 && mkdir -p /data /app/system/migrations \
 && chown -R 10001 /data /app
USER 10001
WORKDIR /app
EXPOSE 8080
# Deliberately NOT `/health`.
#
# Two reasons, and both would otherwise leave containers permanently
# unhealthy for reasons nothing logs:
#
#  1. `.with_health()` mounts `/health` and `/ready` on a Postgres build
#     and silently does nothing on SQLite or MySQL (#1457) — the
#     non-Postgres `runserver` arm never reads the flag.
#  2. On the SaaS app every app route, `/healthz` included, lives on the
#     *tenant* router. A probe with no tenant Host lands on the operator
#     console, which does not serve it — and at boot there may be no
#     tenant to name anyway.
#
# So the check asks the only question it can answer for both apps on all
# three dialects: is the process listening and routing? Any 2xx/3xx/4xx
# means yes. A connection refusal or a 5xx means no.
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=24 \
  CMD curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/ \
      | grep -qE '^[234]' || exit 1
CMD ["app"]
