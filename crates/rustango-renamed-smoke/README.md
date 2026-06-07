# rustango-renamed-smoke

A regression-net workspace member that proves [#142](https://github.com/ujeenet/rustango/issues/142) (proc-macro-crate path resolution) keeps working as the rustango macros evolve.

## What it tests

The Cargo.toml renames the rustango dep to `orm`:

```toml
[dependencies]
orm = { package = "rustango", path = "../rustango", version = "0.42.0" }
```

…then uses every macro entry point through the renamed name and confirms the resulting code compiles + runs.

| Entry point | Test |
|---|---|
| `#[derive(Model)]` (no FK) | `RenamedDemo` — schema lookup, primary-key registration |
| `#[derive(Model)]` (with FK) | `RenamedDemoChild` — `ForeignKey<RenamedDemo>`, `Relation::Fk` registration |
| `#[derive(Serializer)]` | `RenamedDemoSerializer` — `from_model` + `to_value` round-trip |
| `#[derive(Form)]` | `RenamedDemoForm` — `parse(&payload)` with `min_length` / `min` validators |
| `#[derive(ViewSet)]` | `RenamedDemoViewSet` — `router()` method signature compiles, returns `orm::__axum::Router` |
| `Q!()` proc-macro | `Q!(RenamedDemo.name__icontains = "ada")` → `WhereExpr` |
| `embed_migrations!()` | reads `./migrations/0001_initial.json` at compile time |

If any macro emit site regresses to a hardcoded `::rustango::...` path, this crate's build will break — the smoke test runs as part of `cargo test --workspace --all-features` in CI.

## Why renaming works

`#[derive(Model)]` and the other macros emit code that references the rustango crate root. Pre-#142 the path was hardcoded as `::rustango::core::Model` etc., which fails when the consumer renames the dep.

After #142, every macro emit routes through the `rustango_root()` helper in `crates/rustango-macros/src/lib.rs`:

```rust
fn rustango_root() -> TokenStream2 {
    use proc_macro_crate::{crate_name, FoundCrate};
    match crate_name("rustango") {
        Ok(FoundCrate::Itself) => quote!(::rustango),
        Ok(FoundCrate::Name(name)) => {
            let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
            quote!(::#ident)
        }
        Err(_) => quote!(::rustango),
    }
}
```

`proc-macro-crate` reads the consumer's `Cargo.toml` at expansion time and returns the local name. The helper then emits `::orm` (or whatever the consumer chose) at every site.

External-crate paths (`chrono`, `serde`, `tracing`, `uuid`, `axum`, etc.) follow the same pattern: they're re-exported from rustango as `__chrono` / `__serde` / `__tracing` / `__uuid` / `__axum` (doc-hidden), and the macro emits `#root::__chrono::Utc::now()` etc. Downstream consumers don't need direct deps on those crates just to derive Model.

## Why postgres-gated

The smoke crate's body lives in a `#[cfg(feature = "postgres")] mod gated { ... }` block. Two reasons:

1. The macro emits `#[cfg(feature = "postgres")] impl LoadRelated for #Struct` etc. against the CONSUMER's feature flags. Without postgres on the consumer side, those impls don't emit and the trait bounds elsewhere go unsatisfied.
2. The litmus build (`cargo build -p rustango --no-default-features --features sqlite,tenancy`) runs from the workspace root. Under those flags this crate's postgres feature is OFF, so the body compiles to nothing and the litmus build keeps passing.

## Running the smoke test

```bash
# Compile + run the smoke crate's tests.
cargo test --package rustango-renamed-smoke

# Full workspace run (smoke is included).
cargo test --workspace --all-features
```
