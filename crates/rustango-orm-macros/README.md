# rustango-orm-macros

Proc-macros for the standalone `rustango-orm` crate — ORM-only carve-out of [`rustango-macros`](https://crates.io/crates/rustango-macros).

This crate exposes **only** the three proc-macros the ORM core actually needs:

| Macro | What it does |
|-------|--------------|
| `#[derive(Model)]` | Populates `<T>::SCHEMA`, generates inherent methods (`insert` / `save` / `delete` / `find` / `where_*` / `sum` / etc.), and registers the model with the `inventory` crate. |
| `#[derive(Form)]` | Validates user input + parses it into a strongly-typed struct. |
| `embed_migrations!` | Bakes a `migrations/` directory's `.json` files into a `&'static [(name, content)]` slice at compile time. |

The framework-only derives (`Serializer`, `ViewSet`, `Q!`, `#[rustango::main]`) live in [`rustango-macros`](https://crates.io/crates/rustango-macros) and are intentionally **not** re-exported here. Consumers that depend only on `rustango-orm-macros` literally cannot reach them — that's the carve-out half of issue [#143](https://github.com/ujeenet/rustango/issues/143).

## Quickstart

```toml
[dependencies]
rustango-orm-macros = "0.57"
# There is no standalone `rustango-orm` runtime crate — pull in `rustango`:
rustango = { version = "0.57", default-features = false, features = ["sqlite"] }
```

```rust
use rustango::sql::Auto;
use rustango_orm_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "post")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}
```

## Why a separate crate?

The [orm-extract epic (#149)](https://github.com/ujeenet/rustango/issues/149) set out to carve the rustango ORM out of the framework crate, so projects that only want derive-driven models against an existing database could pull in the ORM bits without admin / tenancy / templates / auth.

**That epic is closed and the carve-out was deferred** — the runtime slice ([#144](https://github.com/ujeenet/rustango/issues/144)) is closed too, and no `rustango-orm` crate exists. The blocker is entanglement between m2m and signals/contenttypes, which cannot be split without taking those along.

So this crate is what it is rather than a staging post: a thin re-exporter over `rustango-macros`, useful if you want to name the ORM macros separately. The proc-macro bodies are not migrating here.

## How proc-macro re-export works

Rust's resolver follows `pub use` paths to find a proc-macro's defining crate. Writing `use rustango_orm_macros::Model;` resolves to `pub use rustango_macros::Model;`, walks back to the `proc-macro = true` crate where the derive is actually defined, and invokes its entry point. Because of this resolution chain, the re-exporter itself doesn't need `proc-macro = true` — it's a regular `[lib]` crate.

## License

MIT OR Apache-2.0 — same as [rustango](https://github.com/ujeenet/rustango).
