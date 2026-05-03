# rustango-macros

Proc-macros backing the [`rustango`](https://crates.io/crates/rustango) web framework.

End users normally reach these via the framework's facade (e.g. `use rustango::Model;` or `use rustango::Form;`) — depending on this crate directly is rarely needed. Re-exported macros from the parent crate include:

| Macro | What it does |
|---|---|
| `#[derive(Model)]` | Implements `rustango::core::Model` for a struct, populates the `inventory` registry the auto-admin walks, generates `objects()` / typed columns / `insert` / `delete` / `save`. |
| `#[derive(Form)]` | Implements `rustango::forms::Form` so a struct can be parsed from an HTTP form payload with multi-error validation. Behind the `forms` feature on `rustango`. |
| `#[derive(Serializer)]` | Implements `rustango::serializer::ModelSerializer` for typed JSON output. With the `openapi` feature also emits `OpenApiSchema`. |
| `#[derive(ViewSet)]` | Generates a `router(prefix, pool) -> axum::Router` associated method wiring the full DRF-style CRUD ViewSet in one annotation. |
| `embed_migrations!("path")` | Bakes every migration in a directory into the binary at compile time (single-binary distribution). |
| `#[rustango::main]` | `runserver` entrypoint that wraps `#[tokio::main]` with a default `tracing-subscriber` boot. |

## Features

- `openapi` — additionally emit `impl OpenApiSchema` from `#[derive(Serializer)]` so existing serializers become the source of truth for OpenAPI schemas. Forwarded by the parent crate's `openapi` feature.

## Stability

Internal API of this crate (helpers, intermediate enums, etc.) is **not** part of the public API contract — only the proc-macro entry points and the surface they generate are stable across minor versions. Pin `rustango-macros` exactly when in doubt.

## License

MIT OR Apache-2.0. See the [parent repository](https://github.com/ujeenet/rustango) for the canonical README + full project documentation.
