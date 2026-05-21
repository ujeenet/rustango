//! Proc-macros for rustango.
//!
//! v0.1 ships `#[derive(Model)]`, which emits:
//! * a `Model` impl carrying a static `ModelSchema`,
//! * an `inventory::submit!` so the model is discoverable from the registry,
//! * an inherent `objects()` returning a `QuerySet<Self>`,
//! * a `sqlx::FromRow` impl so query results decode into the struct.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, spanned::Spanned, Data, DeriveInput, Fields, GenericArgument, LitStr,
    PathArguments, Type, TypePath,
};

/// Derive a `Model` impl. See crate docs for the supported attributes.
#[proc_macro_derive(Model, attributes(rustango))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a `router(prefix, pool) -> axum::Router` associated method on a
/// marker struct, wiring the full CRUD ViewSet in one annotation.
///
/// ```ignore
/// #[derive(ViewSet)]
/// #[viewset(
///     model        = Post,
///     fields       = "id, title, body, author_id",
///     filter_fields = "author_id",
///     search_fields = "title, body",
///     ordering     = "-published_at",
///     page_size    = 20,
/// )]
/// pub struct PostViewSet;
///
/// // Mount into your app:
/// let app = Router::new()
///     .merge(PostViewSet::router("/api/posts", pool.clone()));
/// ```
///
/// Attributes:
/// * `model = TypeName` — *required*. The `#[derive(Model)]` struct whose
///   `SCHEMA` constant drives the endpoints.
/// * `fields = "a, b, c"` — scalar fields included in list/retrieve JSON
///   and accepted on create/update (default: all scalar fields).
/// * `filter_fields = "a, b"` — fields filterable via `?a=v` query params.
/// * `search_fields = "a, b"` — fields searched by `?search=...`.
/// * `ordering = "a, -b"` — default list ordering; prefix `-` for DESC.
/// * `page_size = N` — default page size (default: 20, max: 1000).
/// * `read_only` — flag; wires only `list` + `retrieve` (no mutations).
/// * `permissions(list = "...", retrieve = "...", create = "...",
///   update = "...", destroy = "...")` — codenames required per action.
#[proc_macro_derive(ViewSet, attributes(viewset))]
pub fn derive_viewset(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_viewset(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive `rustango::forms::Form` (slice 8.4B). Generates a
/// `parse(&HashMap<String, String>) -> Result<Self, FormErrors>` impl
/// that walks every named field and:
///
/// * Parses the string value into the field's Rust type (`String`,
///   `i32`, `i64`, `f32`, `f64`, `bool`, plus `Option<T>` for the
///   nullable case).
/// * Applies any `#[form(min = ..)]` / `#[form(max = ..)]` /
///   `#[form(min_length = ..)]` / `#[form(max_length = ..)]`
///   validators in declaration order, returning `FormError::Parse`
///   on the first failure.
///
/// Example:
///
/// ```ignore
/// #[derive(Form)]
/// pub struct CreateItemForm {
///     #[form(min_length = 1, max_length = 64)]
///     pub name: String,
///     #[form(min = 0, max = 150)]
///     pub age: i32,
///     pub active: bool,
///     pub email: Option<String>,
/// }
///
/// let parsed = CreateItemForm::parse(&form_map)?;
/// ```
#[proc_macro_derive(Form, attributes(form))]
pub fn derive_form(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_form(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive `rustango::serializer::ModelSerializer` for a struct.
/// (intra-doc link disabled — the macro crate doesn't depend on
/// `rustango` itself, so rustdoc can't resolve the path.)
///
/// # Container attribute (required)
/// `#[serializer(model = TypeName)]` — the [`Model`] type this serializer maps from.
///
/// # Field attributes
/// - `#[serializer(read_only)]` — mapped from model; included in JSON output; excluded from `writable_fields()`
/// - `#[serializer(write_only)]` — `Default::default()` in `from_model`; excluded from JSON output; included in `writable_fields()`
/// - `#[serializer(source = "field_name")]` — reads from `model.field_name` instead of `model.<field_ident>`
/// - `#[serializer(skip)]` — `Default::default()` in `from_model`; included in JSON output; excluded from `writable_fields()` (user sets manually)
/// - `#[serializer(method = "fn_name")]` — DRF `SerializerMethodField`: calls `Self::fn_name(&model)` for the field value; excluded from `writable_fields()`
/// - `#[serializer(nested)]` / `nested(strict)` — auto-resolves nested serializer from a loaded `ForeignKey`; excluded from `writable_fields()`
/// - `#[serializer(many = ChildSerializer)]` — collection of nested serializers; populated via macro-emitted `set_<field>(&[Child::Model])`; excluded from `writable_fields()`
/// - `#[serializer(slug = "name")]` — DRF `SlugRelatedField`: clones `model.<source>.value()?.name`; excluded from `writable_fields()` (v0.44)
/// - `#[serializer(validate = "fn_name")]` — per-field validator surfaced by `Self::validate(&self)`
///
/// The macro also emits a custom `impl serde::Serialize` — do **not** also `#[derive(Serialize)]`.
#[proc_macro_derive(Serializer, attributes(serializer))]
pub fn derive_serializer(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_serializer(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Bake every `*.json` migration file in a directory into the binary
/// at compile time. Returns a `&'static [(&'static str, &'static str)]`
/// of `(name, json_content)` pairs, lex-sorted by file stem.
///
/// Pair with `rustango::migrate::migrate_embedded` at runtime — same
/// behaviour as `migrate(pool, dir)` but with no filesystem access.
/// The path is interpreted relative to the user's `CARGO_MANIFEST_DIR`
/// (i.e. the crate that invokes the macro). Default is
/// `"./migrations"` if no argument is supplied.
///
/// ```ignore
/// const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!();
/// // or:
/// const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("./migrations");
///
/// rustango::migrate::migrate_embedded(&pool, EMBEDDED).await?;
/// ```
///
/// **Compile-time guarantees** (rustango v0.4+, slice 5): every JSON
/// file's `name` field must equal its file stem, every `prev`
/// reference must point to another migration in the same directory,
/// and the JSON must parse. A broken chain — orphan `prev`, missing
/// predecessor, malformed file — fails at macro-expansion time with
/// a clear `compile_error!`. *No other Django-shape Rust framework
/// validates migration chains at compile time*: Cot's migrations are
/// imperative Rust code (no static chain), Loco's are SeaORM
/// up/down (same), Rwf's are raw SQL (no chain at all).
///
/// Each migration is included via `include_str!` so cargo's rebuild
/// detection picks up file *content* changes. **Caveat:** cargo
/// doesn't watch directory listings, so adding or removing a
/// migration file inside the dir won't auto-trigger a rebuild — run
/// `cargo clean` (or just bump any other source file) when you add
/// new migrations during embedded development.
#[proc_macro]
pub fn embed_migrations(input: TokenStream) -> TokenStream {
    expand_embed_migrations(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// `Q!()` — Django-shape filter syntax compile-time-resolved against
/// typed columns. Issue #269 / T1.7.
///
/// Each invocation lowers to the equivalent typed-column method call:
///
/// ```ignore
/// // These expand identically:
/// Q!(User.email__icontains = "alice")
/// User::email.ilike("%alice%")
/// ```
///
/// Field-name typos fail the build (the macro emits `User::no_such_field`
/// which doesn't exist) — the headline ergonomic win of this slice over
/// Django's stringly-typed `__lookup` filters.
///
/// # Supported lookup suffixes
///
/// * bare `=` / `__exact` → `.eq(value)`
/// * `__iexact` → `.ilike(value)` (case-insensitive equality, no wildcards)
/// * `__ne` → `.ne(value)`
/// * `__gt` / `__gte` / `__lt` / `__lte` → corresponding comparison
/// * `__contains` / `__icontains` → `.like("%v%")` / `.ilike("%v%")`
/// * `__startswith` / `__istartswith` → `.like("v%")` / `.ilike("v%")`
/// * `__endswith` / `__iendswith` → `.like("%v")` / `.ilike("%v")`
/// * `__in` → `.is_in(iterable)`
/// * `__not_in` → `.not_in(iterable)`
/// * `__isnull = true` → `.is_null()`; `__isnull = false` → `.is_not_null()`
/// * `__between` accepts a tuple literal `(lo, hi)` → `.between(lo, hi)`
/// * `__regex` / `__iregex` → `.regex(pattern)` / `.iregex(pattern)`
///
/// Unknown suffixes fail the build with a `compile_error!` pointing at
/// the lookup token.
///
/// # Combine
///
/// Each `Q!()` returns a `TypedFilter<Model>` — chain via the existing
/// `.and()` / `.or()` / `.not()` methods:
///
/// ```ignore
/// User::objects()
///     .where_(
///         Q!(User.active = true)
///             .and(Q!(User.email__icontains = "alice"))
///     )
///     .fetch_pool(&pool).await?;
/// ```
///
/// All emitted code routes through existing per-dialect writers — no new
/// SQL emission machinery. Tri-dialect support is inherent.
#[allow(non_snake_case)]
#[proc_macro]
pub fn Q(input: TokenStream) -> TokenStream {
    expand_q(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// `#[rustango::main]` — the Django-shape runserver entrypoint. Wraps
/// `#[tokio::main]` and a default `tracing_subscriber` initialisation
/// (env-filter, falling back to `info,sqlx=warn`) so user `main`
/// functions are zero-boilerplate:
///
/// ```ignore
/// #[rustango::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     rustango::server::Builder::from_env().await?
///         .migrate("migrations").await?
///         .api(my_app::urls::api())
///         .seed_with(my_app::seed::run).await?
///         .serve("0.0.0.0:8080").await
/// }
/// ```
///
/// Optional `flavor = "current_thread"` passes through to
/// `#[tokio::main]`; default is the multi-threaded runtime.
///
/// Pulls `tracing-subscriber` into the rustango crate behind the
/// `runtime` sub-feature (implied by `tenancy`), so apps that opt
/// out get plain `#[tokio::main]` ergonomics without the dependency.
#[proc_macro_attribute]
pub fn main(args: TokenStream, item: TokenStream) -> TokenStream {
    expand_main(args.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_main(args: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let mut input: syn::ItemFn = syn::parse2(item)?;
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new(
            input.sig.ident.span(),
            "`#[rustango::main]` must wrap an `async fn`",
        ));
    }

    // v0.31.1 (#4): hand-roll the tokio runtime instead of delegating
    // to `#[tokio::main]`. Tokio's proc-macro internally emits
    // `::tokio::*` paths that resolve against the user crate's deps,
    // so calling it through the rustango re-export still requires the
    // user to add tokio to their own Cargo.toml. Building the
    // runtime ourselves keeps the dep transitive through the
    // `runtime` feature on rustango.
    //
    // Parse optional `flavor = "current_thread"` / `flavor =
    // "multi_thread"` from the attribute args. Unknown args are
    // tolerated (forward-compat with tokio's own arg surface).
    let flavor = parse_flavor(&args);
    let builder_call = match flavor {
        Flavor::CurrentThread => quote! {
            ::rustango::__private_runtime::tokio::runtime::Builder::new_current_thread()
        },
        Flavor::MultiThread => quote! {
            ::rustango::__private_runtime::tokio::runtime::Builder::new_multi_thread()
        },
    };

    // Detach the user body and rewrite `main` as a sync fn that
    // builds the runtime and blocks on the async body.
    let user_body = input.block.clone();
    input.sig.asyncness = None;
    input.block = syn::parse2(quote! {{
        {
            use ::rustango::__private_runtime::tracing_subscriber::{self, EnvFilter};
            // `try_init` so duplicate installers (e.g. tests already
            // holding a subscriber) don't panic.
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
                )
                .try_init();
        }
        let __rt = #builder_call
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        __rt.block_on(async move #user_body)
    }})?;

    Ok(quote! {
        #input
    })
}

enum Flavor {
    MultiThread,
    CurrentThread,
}

fn parse_flavor(args: &TokenStream2) -> Flavor {
    // Cheap parser: look for the literal token sequence
    // `flavor = "current_thread"`. Everything else (including
    // bare `multi_thread` or no args) defaults to multi-thread.
    let s = args.to_string();
    if s.contains("current_thread") {
        Flavor::CurrentThread
    } else {
        Flavor::MultiThread
    }
}

/// Parse form for `Q!()` — `<TypePath>.<Ident> = <Expr>`.
struct QInput {
    base_path: syn::Path,
    field: syn::Ident,
    value: syn::Expr,
}

impl syn::parse::Parse for QInput {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let base_path: syn::Path = input.parse()?;
        input.parse::<syn::Token![.]>()?;
        let field: syn::Ident = input.parse()?;
        input.parse::<syn::Token![=]>()?;
        let value: syn::Expr = input.parse()?;
        Ok(QInput {
            base_path,
            field,
            value,
        })
    }
}

fn expand_q(input: TokenStream2) -> syn::Result<TokenStream2> {
    let q: QInput = syn::parse2(input)?;
    let field_str = q.field.to_string();
    let field_span = q.field.span();
    let (base, suffix) = match field_str.find("__") {
        Some(idx) => (&field_str[..idx], &field_str[idx + 2..]),
        None => (field_str.as_str(), ""),
    };
    if base.is_empty() {
        return Err(syn::Error::new(
            field_span,
            "Q!(): field name is empty before `__` suffix",
        ));
    }
    let base_ident = syn::Ident::new(base, field_span);
    let value = &q.value;
    let path = &q.base_path;

    // Most suffixes map directly to a Column method with the value
    // forwarded unchanged. Some need value-shape massaging (wildcards
    // for LIKE-family, tuple destructure for BETWEEN, literal-bool for
    // ISNULL). Unknown suffixes fail the build.
    let expanded = match suffix {
        "" | "exact" => quote! {
            ::rustango::core::Column::eq(#path::#base_ident, #value)
        },
        "ne" => quote! {
            ::rustango::core::Column::ne(#path::#base_ident, #value)
        },
        "gt" => quote! {
            ::rustango::core::Column::gt(#path::#base_ident, #value)
        },
        "gte" => quote! {
            ::rustango::core::Column::gte(#path::#base_ident, #value)
        },
        "lt" => quote! {
            ::rustango::core::Column::lt(#path::#base_ident, #value)
        },
        "lte" => quote! {
            ::rustango::core::Column::lte(#path::#base_ident, #value)
        },
        "iexact" => quote! {
            // Django emulates `__iexact` as case-insensitive equality.
            // The non-wildcard `ILIKE value` is semantically identical
            // for plain strings; LIKE-metachars `%` `_` in the rhs would
            // accidentally match more — document the caveat.
            ::rustango::core::Column::ilike(#path::#base_ident, ::std::string::ToString::to_string(&(#value)))
        },
        "contains" => quote! {
            ::rustango::core::Column::like(
                #path::#base_ident,
                ::std::format!("%{}%", #value),
            )
        },
        "icontains" => quote! {
            ::rustango::core::Column::ilike(
                #path::#base_ident,
                ::std::format!("%{}%", #value),
            )
        },
        "startswith" => quote! {
            ::rustango::core::Column::like(
                #path::#base_ident,
                ::std::format!("{}%", #value),
            )
        },
        "istartswith" => quote! {
            ::rustango::core::Column::ilike(
                #path::#base_ident,
                ::std::format!("{}%", #value),
            )
        },
        "endswith" => quote! {
            ::rustango::core::Column::like(
                #path::#base_ident,
                ::std::format!("%{}", #value),
            )
        },
        "iendswith" => quote! {
            ::rustango::core::Column::ilike(
                #path::#base_ident,
                ::std::format!("%{}", #value),
            )
        },
        "in" => quote! {
            ::rustango::core::Column::is_in(#path::#base_ident, #value)
        },
        "not_in" => quote! {
            ::rustango::core::Column::not_in(#path::#base_ident, #value)
        },
        "isnull" => {
            // Must be a bool literal at macro time so we can route to
            // is_null vs is_not_null without a runtime branch.
            let b = match value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Bool(b),
                    ..
                }) => b.value(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        value,
                        "Q!(): `__isnull` requires a `true` or `false` literal",
                    ));
                }
            };
            if b {
                quote! { ::rustango::core::Column::is_null(#path::#base_ident) }
            } else {
                quote! { ::rustango::core::Column::is_not_null(#path::#base_ident) }
            }
        }
        "between" => {
            // Accept a tuple literal `(lo, hi)`.
            let tuple = match value {
                syn::Expr::Tuple(t) if t.elems.len() == 2 => t,
                _ => {
                    return Err(syn::Error::new_spanned(
                        value,
                        "Q!(): `__between` requires a tuple literal `(lo, hi)`",
                    ));
                }
            };
            let lo = &tuple.elems[0];
            let hi = &tuple.elems[1];
            quote! { ::rustango::core::Column::between(#path::#base_ident, #lo, #hi) }
        }
        "regex" => quote! {
            ::rustango::core::Column::regex(#path::#base_ident, #value)
        },
        "iregex" => quote! {
            ::rustango::core::Column::iregex(#path::#base_ident, #value)
        },
        _ => {
            return Err(syn::Error::new(
                field_span,
                format!(
                    "Q!(): unknown lookup suffix `__{}`. Supported: __exact / __iexact / __ne / __gt / __gte / __lt / __lte / __contains / __icontains / __startswith / __istartswith / __endswith / __iendswith / __in / __not_in / __isnull / __between / __regex / __iregex",
                    suffix
                ),
            ));
        }
    };
    Ok(expanded)
}

fn expand_embed_migrations(input: TokenStream2) -> syn::Result<TokenStream2> {
    // Default to "./migrations" if invoked without args.
    let path_str = if input.is_empty() {
        "./migrations".to_string()
    } else {
        let lit: LitStr = syn::parse2(input)?;
        lit.value()
    };

    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "embed_migrations! must be invoked during a Cargo build (CARGO_MANIFEST_DIR not set)",
        )
    })?;
    let abs = std::path::Path::new(&manifest).join(&path_str);

    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    if abs.is_dir() {
        let read = std::fs::read_dir(&abs).map_err(|e| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!("embed_migrations!: cannot read {}: {e}", abs.display()),
            )
        })?;
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            entries.push((stem.to_owned(), path));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Compile-time chain validation: read each migration's JSON,
    // pull `name` and `prev` (file-stem-keyed for the chain check),
    // and verify every `prev` points to another migration in the
    // slice. Mismatches between the file stem and the embedded
    // `name` field — and broken `prev` chains — fail at MACRO
    // EXPANSION time so a misshapen migration set never compiles.
    //
    // This is the v0.4 Slice 5 distinguisher: rustango's JSON
    // migrations + a Rust proc-macro that reads them is the unique
    // combo nothing else in the Django-shape Rust camp can match
    // (Cot's are imperative Rust code, Loco's are SeaORM up/down,
    // Rwf's are raw SQL — none have a static chain to validate).
    let mut chain_names: Vec<String> = Vec::with_capacity(entries.len());
    let mut prev_refs: Vec<(String, Option<String>)> = Vec::with_capacity(entries.len());
    for (stem, path) in &entries {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "embed_migrations!: cannot read {} for chain validation: {e}",
                    path.display()
                ),
            )
        })?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "embed_migrations!: {} is not valid JSON: {e}",
                    path.display()
                ),
            )
        })?;
        let name = json
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                syn::Error::new(
                    proc_macro2::Span::call_site(),
                    format!(
                        "embed_migrations!: {} is missing the `name` field",
                        path.display()
                    ),
                )
            })?
            .to_owned();
        if name != *stem {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "embed_migrations!: file stem `{stem}` does not match the migration's \
                     `name` field `{name}` — rename the file or fix the JSON",
                ),
            ));
        }
        let prev = json.get("prev").and_then(|v| v.as_str()).map(str::to_owned);
        chain_names.push(name.clone());
        prev_refs.push((name, prev));
    }

    let name_set: std::collections::HashSet<&str> =
        chain_names.iter().map(String::as_str).collect();
    for (name, prev) in &prev_refs {
        if let Some(p) = prev {
            if !name_set.contains(p.as_str()) {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    format!(
                        "embed_migrations!: broken migration chain — `{name}` declares \
                         prev=`{p}` but no migration with that name exists in {}",
                        abs.display()
                    ),
                ));
            }
        }
    }

    let pairs: Vec<TokenStream2> = entries
        .iter()
        .map(|(name, path)| {
            let path_lit = path.display().to_string();
            quote! { (#name, ::core::include_str!(#path_lit)) }
        })
        .collect();

    Ok(quote! {
        {
            const __RUSTANGO_EMBEDDED: &[(&'static str, &'static str)] = &[#(#pairs),*];
            __RUSTANGO_EMBEDDED
        }
    })
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Model can only be derived on structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Model requires a struct with named fields",
        ));
    };

    let container = parse_container_attrs(input)?;
    let table = container
        .table
        .unwrap_or_else(|| to_snake_case(&struct_name.to_string()));
    let model_name = struct_name.to_string();

    let collected = collect_fields(named, &table)?;

    // Validate that #[rustango(display = "…")] names a real field.
    if let Some((ref display, span)) = container.display {
        if !collected.field_names.iter().any(|n| n == display) {
            return Err(syn::Error::new(
                span,
                format!("`display = \"{display}\"` does not match any field on this struct"),
            ));
        }
    }
    let display = container.display.map(|(name, _)| name);
    let app_label = container.app.clone();

    // Validate admin field-name lists against declared field names.
    // Note: `list_display` is intentionally NOT validated here. As of
    // v0.32 it may also reference inventory-registered computed
    // fields (via `register_admin_computed!`) whose existence the
    // macro can't see at compile time — they're submitted from any
    // crate that depends on rustango. The runtime list-view resolves
    // unknown names against the inventory + silently drops the
    // truly-bogus ones, which is the cheaper trade-off versus
    // forcing a per-Model attr to opt out.
    if let Some(admin) = &container.admin {
        for (label, list) in [
            ("search_fields", &admin.search_fields),
            ("readonly_fields", &admin.readonly_fields),
            ("list_filter", &admin.list_filter),
        ] {
            if let Some((names, span)) = list {
                for name in names {
                    if !collected.field_names.iter().any(|n| n == name) {
                        return Err(syn::Error::new(
                            *span,
                            format!(
                                "`{label} = \"{name}\"`: \"{name}\" is not a declared field on this struct"
                            ),
                        ));
                    }
                }
            }
        }
        if let Some((pairs, span)) = &admin.ordering {
            for (name, _) in pairs {
                if !collected.field_names.iter().any(|n| n == name) {
                    return Err(syn::Error::new(
                        *span,
                        format!(
                            "`ordering = \"{name}\"`: \"{name}\" is not a declared field on this struct"
                        ),
                    ));
                }
            }
        }
        if let Some((groups, span)) = &admin.fieldsets {
            for (_, fields) in groups {
                for name in fields {
                    if !collected.field_names.iter().any(|n| n == name) {
                        return Err(syn::Error::new(
                            *span,
                            format!(
                                "`fieldsets`: \"{name}\" is not a declared field on this struct"
                            ),
                        ));
                    }
                }
            }
        }
    }
    if let Some(audit) = &container.audit {
        if let Some((names, span)) = &audit.track {
            for name in names {
                if !collected.field_names.iter().any(|n| n == name) {
                    return Err(syn::Error::new(
                        *span,
                        format!(
                            "`audit(track = \"{name}\")`: \"{name}\" is not a declared field on this struct"
                        ),
                    ));
                }
            }
        }
    }

    // Issue #291 / T2.5 — validate each `default_order` column name
    // against the model's collected fields. Typos fail at macro-expand
    // time, not at the database.
    for (col, _desc, span) in &container.default_order {
        if !collected.field_names.iter().any(|n| n == col) {
            return Err(syn::Error::new(
                *span,
                format!(
                    "`default_order = \"...\"`: \"{col}\" is not a declared field on this struct"
                ),
            ));
        }
    }

    // Build the audit_track list for ModelSchema: None when no audit attr,
    // Some(empty) when audit present without track, Some(names) when explicit.
    let audit_track_names: Option<Vec<String>> = container.audit.as_ref().map(|audit| {
        audit
            .track
            .as_ref()
            .map(|(names, _)| names.clone())
            .unwrap_or_default()
    });

    // Merge field-level indexes into the container's index list.
    let mut all_indexes: Vec<IndexAttr> = container.indexes;
    for field in &named.named {
        let ident = field.ident.as_ref().expect("named");
        let col = to_snake_case(&ident.to_string()); // column name fallback
                                                     // Re-parse field attrs to check for index flag
        if let Ok(fa) = parse_field_attrs(field) {
            if fa.index {
                let col_name = fa.column.clone().unwrap_or_else(|| col.clone());
                let auto_name = if fa.index_unique {
                    format!("{table}_{col_name}_uq_idx")
                } else {
                    format!("{table}_{col_name}_idx")
                };
                all_indexes.push(IndexAttr {
                    name: fa.index_name.or(Some(auto_name)),
                    columns: vec![col_name],
                    unique: fa.index_unique,
                    method: fa.index_method,
                    where_clause: None,
                });
            }
        }
    }

    let model_impl = model_impl_tokens(
        struct_name,
        &model_name,
        &table,
        display.as_deref(),
        app_label.as_deref(),
        container.admin.as_ref(),
        &container.default_order,
        &collected.field_schemas,
        collected.soft_delete_column.as_deref(),
        container.permissions,
        audit_track_names.as_deref(),
        &container.m2m,
        &all_indexes,
        &container.checks,
        &container.composite_fks,
        &container.generic_fks,
        container.scope.as_deref(),
        container.is_view,
        container.verbose_name.as_deref(),
        container.verbose_name_plural.as_deref(),
    );
    let module_ident = column_module_ident(struct_name);
    let column_consts = column_const_tokens(&module_ident, &collected.column_entries);
    let audited_fields: Option<Vec<&ColumnEntry>> = container.audit.as_ref().map(|audit| {
        let track_set: Option<std::collections::HashSet<&str>> = audit
            .track
            .as_ref()
            .map(|(names, _)| names.iter().map(String::as_str).collect());
        collected
            .column_entries
            .iter()
            .filter(|c| {
                track_set
                    .as_ref()
                    .map_or(true, |s| s.contains(c.name.as_str()))
            })
            .collect()
    });
    let inherent_impl = inherent_impl_tokens(
        struct_name,
        &collected,
        collected.primary_key.as_ref(),
        &column_consts,
        audited_fields.as_deref(),
        &all_indexes,
        &container.manager_fns,
    );
    let column_module = column_module_tokens(&module_ident, struct_name, &collected.column_entries);
    let from_row_impl = from_row_impl_tokens(struct_name, &collected.from_row_inits);
    let reverse_helpers = reverse_helper_tokens(struct_name, &collected.fk_relations);
    let m2m_accessors = m2m_accessor_tokens(struct_name, &container.m2m);
    let generic_fk_accessors = generic_fk_accessor_tokens(
        struct_name,
        &container.generic_fks,
        &collected.column_entries,
    );

    // Issue #271 / T1.9 — `#[rustango(manager(ext = "FooManagerExt"))]`
    // emits an empty extension trait so users can add methods via
    // `impl FooManagerExt for QuerySet<Foo>` without hand-writing the
    // trait declaration. See `crates/rustango/src/manager.rs` for the
    // pattern this replaces.
    let manager_trait = container.manager_ext.as_ref().map(|name| {
        let model_name_str = struct_name.to_string();
        let doc = format!(
            "Custom-Manager extension trait for [`{model_name_str}`]. \
             Generated by `#[rustango(manager(ext = ...))]`. Add methods \
             via `impl {name} for QuerySet<{model_name_str}> {{ ... }}`."
        );
        quote! {
            #[doc = #doc]
            pub trait #name: ::core::marker::Sized {}
        }
    });

    Ok(quote! {
        #model_impl
        #inherent_impl
        #from_row_impl
        #column_module
        #reverse_helpers
        #m2m_accessors
        #generic_fk_accessors
        #manager_trait

        ::rustango::core::inventory::submit! {
            ::rustango::core::ModelEntry {
                schema: <#struct_name as ::rustango::core::Model>::SCHEMA,
                // `module_path!()` evaluates at the registration site,
                // so a Model declared in `crate::blog::models` records
                // `"<crate>::blog::models"` and `resolved_app_label()`
                // can infer "blog" without an explicit attribute.
                module_path: ::core::module_path!(),
            }
        }
    })
}

/// Emit `impl LoadRelated for #StructName` — slice 9.0d. Pattern-
/// matches `field_name` against the model's FK fields and, for a
/// match, decodes the FK target via the parent's macro-generated
/// `__rustango_from_aliased_row`, reads the parent's PK, and stores
/// `ForeignKey::Loaded` on `self`.
///
/// Always emitted (with empty arms for FK-less models, which
/// return `Ok(false)` for any field name) so the `T: LoadRelated`
/// trait bound on `fetch_on` is universally satisfied — users
/// never have to think about implementing it.
fn load_related_impl_tokens(struct_name: &syn::Ident, fk_relations: &[FkRelation]) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let parent_ty = &rel.parent_type;
        let fk_col = rel.fk_column.as_str();
        // FK field's Rust ident matches its SQL column name in v0.8
        // (no `column = "..."` rename ships on FK fields).
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        let (variant_ident, default_expr) = rel.pk_kind.sqlvalue_match_arm();
        let assign = if rel.nullable {
            quote! {
                self.#field_ident = ::core::option::Option::Some(
                    ::rustango::sql::ForeignKey::loaded(_pk, _parent),
                );
            }
        } else {
            quote! {
                self.#field_ident = ::rustango::sql::ForeignKey::loaded(_pk, _parent);
            }
        };
        quote! {
            #fk_col => {
                let _parent: #parent_ty = <#parent_ty>::__rustango_from_aliased_row(row, alias)?;
                // Loud-in-debug, default-in-release: a divergence
                // between the FK field's declared `K` (drives the
                // expected `SqlValue::<Variant>`) and the parent's
                // `__rustango_pk_value` output is a macro-internal
                // invariant break — surfacing the panic in dev
                // catches it before users hit silent PK=0 corruption.
                let _pk = match <#parent_ty>::__rustango_pk_value(&_parent) {
                    ::rustango::core::SqlValue::#variant_ident(v) => v,
                    _other => {
                        ::core::debug_assert!(
                            false,
                            "rustango macro bug: load_related on FK `{}` expected \
                             SqlValue::{} from parent's __rustango_pk_value but got \
                             {:?} — file a bug at https://github.com/ujeenet/rustango",
                            #fk_col,
                            ::core::stringify!(#variant_ident),
                            _other,
                        );
                        #default_expr
                    }
                };
                #assign
                ::core::result::Result::Ok(true)
            }
        }
    });
    quote! {
        #[cfg(feature = "postgres")]
        impl ::rustango::sql::LoadRelated for #struct_name {
            #[allow(unused_variables)]
            fn __rustango_load_related(
                &mut self,
                row: &::rustango::sql::sqlx::postgres::PgRow,
                field_name: &str,
                alias: &str,
            ) -> ::core::result::Result<bool, ::rustango::sql::sqlx::Error> {
                match field_name {
                    #( #arms )*
                    _ => ::core::result::Result::Ok(false),
                }
            }
        }
    }
}

/// MySQL counterpart of [`load_related_impl_tokens`] — v0.23.0-batch8.
/// Emits a call to the cfg-gated `__impl_my_load_related!` macro_rules,
/// which expands to a `LoadRelatedMy` impl when rustango is built with
/// the `mysql` feature, and to nothing otherwise. The decoded parent
/// is read via `__rustango_from_aliased_my_row` (the MySQL aliased
/// decoder, also batch8) so the dual emission is symmetric across
/// backends.
fn load_related_impl_my_tokens(
    struct_name: &syn::Ident,
    fk_relations: &[FkRelation],
) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let parent_ty = &rel.parent_type;
        let fk_col = rel.fk_column.as_str();
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        let (variant_ident, default_expr) = rel.pk_kind.sqlvalue_match_arm();
        let assign = if rel.nullable {
            quote! {
                __self.#field_ident = ::core::option::Option::Some(
                    ::rustango::sql::ForeignKey::loaded(_pk, _parent),
                );
            }
        } else {
            quote! {
                __self.#field_ident = ::rustango::sql::ForeignKey::loaded(_pk, _parent);
            }
        };
        // `self` IS hygiene-tracked through macro_rules — emitted from
        // a different context than the `&mut self` parameter inside
        // the macro_rules-expanded fn. Pass it through as `__self`
        // and let the macro_rules rebind it to the receiver.
        quote! {
            #fk_col => {
                let _parent: #parent_ty =
                    <#parent_ty>::__rustango_from_aliased_my_row(row, alias)?;
                // See note in `load_related_impl_tokens` (PG twin) —
                // the same loud-in-debug invariant guard.
                let _pk = match <#parent_ty>::__rustango_pk_value(&_parent) {
                    ::rustango::core::SqlValue::#variant_ident(v) => v,
                    _other => {
                        ::core::debug_assert!(
                            false,
                            "rustango macro bug: load_related on FK `{}` expected \
                             SqlValue::{} from parent's __rustango_pk_value but got \
                             {:?} — file a bug at https://github.com/ujeenet/rustango",
                            #fk_col,
                            ::core::stringify!(#variant_ident),
                            _other,
                        );
                        #default_expr
                    }
                };
                #assign
                ::core::result::Result::Ok(true)
            }
        }
    });
    quote! {
        ::rustango::__impl_my_load_related!(#struct_name, |__self, row, field_name, alias| {
            #( #arms )*
        });
    }
}

/// Same shape as [`load_related_impl_my_tokens`] but for SQLite.
/// Emits a call to `__impl_sqlite_load_related!` which expands to a
/// `LoadRelatedSqlite` impl when the `sqlite` feature is on.
fn load_related_impl_sqlite_tokens(
    struct_name: &syn::Ident,
    fk_relations: &[FkRelation],
) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let parent_ty = &rel.parent_type;
        let fk_col = rel.fk_column.as_str();
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        let (variant_ident, default_expr) = rel.pk_kind.sqlvalue_match_arm();
        let assign = if rel.nullable {
            quote! {
                __self.#field_ident = ::core::option::Option::Some(
                    ::rustango::sql::ForeignKey::loaded(_pk, _parent),
                );
            }
        } else {
            quote! {
                __self.#field_ident = ::rustango::sql::ForeignKey::loaded(_pk, _parent);
            }
        };
        quote! {
            #fk_col => {
                let _parent: #parent_ty =
                    <#parent_ty>::__rustango_from_aliased_sqlite_row(row, alias)?;
                let _pk = match <#parent_ty>::__rustango_pk_value(&_parent) {
                    ::rustango::core::SqlValue::#variant_ident(v) => v,
                    _other => {
                        ::core::debug_assert!(
                            false,
                            "rustango macro bug: load_related on FK `{}` expected \
                             SqlValue::{} from parent's __rustango_pk_value but got \
                             {:?} — file a bug at https://github.com/ujeenet/rustango",
                            #fk_col,
                            ::core::stringify!(#variant_ident),
                            _other,
                        );
                        #default_expr
                    }
                };
                #assign
                ::core::result::Result::Ok(true)
            }
        }
    });
    quote! {
        ::rustango::__impl_sqlite_load_related!(#struct_name, |__self, row, field_name, alias| {
            #( #arms )*
        });
    }
}

/// Emit `impl FkPkAccess for #StructName` — slice 9.0e. Pattern-
/// matches `field_name` against the model's FK fields and returns
/// the FK's stored PK as `i64`. Used by `fetch_with_prefetch` to
/// group children by parent PK.
///
/// Always emitted (with `_ => None` for FK-less models) so the
/// trait bound on `fetch_with_prefetch` is universally satisfied.
fn fk_pk_access_impl_tokens(struct_name: &syn::Ident, fk_relations: &[FkRelation]) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let fk_col = rel.fk_column.as_str();
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        if rel.pk_kind == DetectedKind::I64 {
            // i64 FK — return the stored PK so prefetch_related can
            // group children by it. Nullable variant unwraps via
            // `as_ref().map(...)`: an unset (NULL) FK column yields
            // `None` and that child sits out of the grouping (correct
            // semantics — it has no parent to attach to).
            if rel.nullable {
                quote! {
                    #fk_col => self.#field_ident
                        .as_ref()
                        .map(|fk| ::rustango::sql::ForeignKey::pk(fk)),
                }
            } else {
                quote! {
                    #fk_col => ::core::option::Option::Some(self.#field_ident.pk()),
                }
            }
        } else {
            // Non-i64 FK PKs (e.g. `ForeignKey<T, String>`,
            // `ForeignKey<T, Uuid>`) opt out of `prefetch_related`'s
            // i64-keyed grouping path — the trait signature is
            // `Option<i64>` and a non-i64 PK can't lower into it.
            // The FK still works for everything else (CRUD, lazy
            // load via `.get()`, select_related JOINs); only the
            // bulk prefetch grouper needs the integer key.
            quote! {
                #fk_col => ::core::option::Option::None,
            }
        }
    });
    // PK-type-agnostic version: every FK arm emits an
    // `Option<SqlValue>` so `fetch_with_prefetch` can group by any
    // PK type (i64, i32, String, Uuid). Models with non-i64 FK PKs
    // opt OUT of the legacy i64 method (it returns None) but opt IN
    // here.
    let value_arms = fk_relations.iter().map(|rel| {
        let fk_col = rel.fk_column.as_str();
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        if rel.nullable {
            quote! {
                #fk_col => self.#field_ident
                    .as_ref()
                    .map(|fk| ::core::convert::Into::<::rustango::core::SqlValue>::into(
                        ::rustango::sql::ForeignKey::pk(fk)
                    )),
            }
        } else {
            quote! {
                #fk_col => ::core::option::Option::Some(
                    ::core::convert::Into::<::rustango::core::SqlValue>::into(
                        self.#field_ident.pk()
                    )
                ),
            }
        }
    });
    quote! {
        impl ::rustango::sql::FkPkAccess for #struct_name {
            #[allow(unused_variables)]
            fn __rustango_fk_pk(&self, field_name: &str) -> ::core::option::Option<i64> {
                match field_name {
                    #( #arms )*
                    _ => ::core::option::Option::None,
                }
            }
            #[allow(unused_variables)]
            fn __rustango_fk_pk_value(
                &self,
                field_name: &str,
            ) -> ::core::option::Option<::rustango::core::SqlValue> {
                match field_name {
                    #( #value_arms )*
                    _ => ::core::option::Option::None,
                }
            }
        }
    }
}

/// For every `ForeignKey<Parent>` field on `Child`, emit
/// `impl Parent { pub async fn <child_table>_set(&self, executor) -> Vec<Child> }`.
/// Reads the parent's PK via the macro-generated `__rustango_pk_value`
/// and runs a single `SELECT … FROM <child_table> WHERE <fk_column> = $1`
/// — the canonical reverse-FK fetch. One round trip, no N+1.
///
/// **PG-only emission**: the accessor is bounded on
/// `sqlx::Executor<Database = sqlx::Postgres>` and calls `fetch_on`,
/// both of which are gated behind the `postgres` cargo feature. The
/// emitted code is wrapped in `#[cfg(feature = "postgres")]` so the
/// model derive itself compiles on tri-dialect / sqlite-only
/// downstream builds — the accessor just isn't materialised. A tri-
/// dialect `_set_pool` variant is a separate follow-up.
fn reverse_helper_tokens(child_ident: &syn::Ident, fk_relations: &[FkRelation]) -> TokenStream2 {
    if fk_relations.is_empty() {
        return TokenStream2::new();
    }
    // Snake-case the child struct name to derive the method suffix —
    // `Post` → `post_set`, `BlogComment` → `blog_comment_set`. Avoids
    // English-plural edge cases (Django's `<child>_set` convention).
    let suffix = format!("{}_set", to_snake_case(&child_ident.to_string()));
    let method_ident = syn::Ident::new(&suffix, child_ident.span());
    let impls = fk_relations.iter().map(|rel| {
        let parent_ty = &rel.parent_type;
        let fk_col = rel.fk_column.as_str();
        let doc = format!(
            "Fetch every `{child_ident}` whose `{fk_col}` foreign key points at this row. \
             Single SQL query — `SELECT … FROM <{child_ident} table> WHERE {fk_col} = $1` — \
             generated from the FK declaration on `{child_ident}::{fk_col}`. Composes with \
             further `{child_ident}::objects()` filters via direct queryset use."
        );
        quote! {
            #[cfg(feature = "postgres")]
            impl #parent_ty {
                #[doc = #doc]
                ///
                /// # Errors
                /// Returns [`::rustango::sql::ExecError`] for SQL-writing
                /// or driver failures.
                pub async fn #method_ident<'_c, _E>(
                    &self,
                    _executor: _E,
                ) -> ::core::result::Result<
                    ::std::vec::Vec<#child_ident>,
                    ::rustango::sql::ExecError,
                >
                where
                    _E: ::rustango::sql::sqlx::Executor<
                        '_c,
                        Database = ::rustango::sql::sqlx::Postgres,
                    >,
                {
                    let _pk: ::rustango::core::SqlValue = self.__rustango_pk_value();
                    ::rustango::query::QuerySet::<#child_ident>::new()
                        .filter_op(#fk_col, ::rustango::core::Op::Eq, _pk)
                        .fetch_on(_executor)
                        .await
                }
            }
        }
    });
    quote! { #( #impls )* }
}

/// Emit `<name>_m2m(&self) -> M2MManager` inherent methods for every M2M
/// relation declared on the model.
/// Emit `{name}_pool` accessor + `set_{name}_for` setter for every
/// `#[rustango(generic_fk(name, ct_column, pk_column))]` declaration.
///
/// Closes #239 + #240 — the Django-shape `comment.content_object` /
/// `comment.content_object = post` ergonomics on top of the existing
/// `GenericForeignKey { content_type_id, object_pk }` primitive.
///
/// `column_entries` is passed so we can resolve each `ct_column` /
/// `pk_column` SQL name back to its Rust field ident — the macro
/// only sees the column-side strings in the attribute, but the
/// emitted accessor needs to read the actual struct field.
fn generic_fk_accessor_tokens(
    struct_name: &syn::Ident,
    generic_fks: &[GenericFkAttr],
    column_entries: &[ColumnEntry],
) -> TokenStream2 {
    if generic_fks.is_empty() {
        return TokenStream2::new();
    }
    let methods = generic_fks.iter().filter_map(|gfk| {
        // Resolve `ct_column` + `pk_column` to the struct's Rust
        // field idents. A typo (column name doesn't match any field)
        // emits no method for that registration — the user will see
        // the compiler reject the SCHEMA literal anyway, so there's
        // a clear error path without us double-reporting.
        let ct_ident = column_entries
            .iter()
            .find(|c| c.column == gfk.ct_column)
            .map(|c| c.ident.clone())?;
        let pk_ident = column_entries
            .iter()
            .find(|c| c.column == gfk.pk_column)
            .map(|c| c.ident.clone())?;

        let accessor_ident =
            syn::Ident::new(&format!("{}_pool", gfk.name), struct_name.span());
        let setter_ident =
            syn::Ident::new(&format!("set_{}_for", gfk.name), struct_name.span());
        let name_literal = gfk.name.as_str();

        Some(quote! {
            #[doc = concat!(
                "Resolve the polymorphic `",
                #name_literal,
                "` relation. Reads `self.",
                stringify!(#ct_ident),
                "` + `self.",
                stringify!(#pk_ident),
                "`, looks up the matching `ContentType`, and fetches the target row as a JSON map.\n\n",
                "Returns `Ok(None)` when the ContentType is stale / unseeded or the target row was deleted. Emitted by `#[rustango(generic_fk(name = \"",
                #name_literal,
                "\", ...))]`."
            )]
            pub async fn #accessor_ident(
                &self,
                pool: &::rustango::sql::Pool,
            ) -> ::core::result::Result<
                ::core::option::Option<::serde_json::Value>,
                ::rustango::sql::ExecError,
            > {
                let gfk = ::rustango::contenttypes::GenericForeignKey::new(
                    self.#ct_ident as i64,
                    self.#pk_ident as i64,
                );
                gfk.get_object(pool).await
            }

            #[doc = concat!(
                "Set the polymorphic `",
                #name_literal,
                "` target. Looks up the `ContentType` for `T` via the cached registry, then assigns both `self.",
                stringify!(#ct_ident),
                "` and `self.",
                stringify!(#pk_ident),
                "`.\n\nFollow with `self.insert(pool)` or `self.update(pool)` to persist. Emitted by `#[rustango(generic_fk(name = \"",
                #name_literal,
                "\", ...))]`."
            )]
            pub async fn #setter_ident<T: ::rustango::core::Model>(
                &mut self,
                pool: &::rustango::sql::Pool,
                target_pk: i64,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                let gfk = ::rustango::contenttypes::GenericForeignKey::for_target::<T>(
                    pool,
                    target_pk,
                ).await?;
                self.#ct_ident = gfk.content_type_id as _;
                self.#pk_ident = gfk.object_pk as _;
                ::core::result::Result::Ok(())
            }
        })
    });
    quote! {
        impl #struct_name {
            #( #methods )*
        }
    }
}

fn m2m_accessor_tokens(struct_name: &syn::Ident, m2m_relations: &[M2MAttr]) -> TokenStream2 {
    if m2m_relations.is_empty() {
        return TokenStream2::new();
    }
    let methods = m2m_relations.iter().map(|rel| {
        let method_name = format!("{}_m2m", rel.name);
        let method_ident = syn::Ident::new(&method_name, struct_name.span());
        let through = rel.through.as_str();
        let src_col = rel.src.as_str();
        let dst_col = rel.dst.as_str();
        quote! {
            pub fn #method_ident(&self) -> ::rustango::sql::M2MManager {
                ::rustango::sql::M2MManager {
                    src_pk: self.__rustango_pk_value(),
                    through: #through,
                    src_col: #src_col,
                    dst_col: #dst_col,
                }
            }
        }
    });
    quote! {
        impl #struct_name {
            #( #methods )*
        }
    }
}

struct ColumnEntry {
    /// The struct field ident, used both for the inherent const name on
    /// the model and for the inner column type's name.
    ident: syn::Ident,
    /// The struct's field type, used as `Column::Value`.
    value_ty: Type,
    /// Rust-side field name (e.g. `"id"`).
    name: String,
    /// SQL-side column name (e.g. `"user_id"`).
    column: String,
    /// `::rustango::core::FieldType::I64` etc.
    field_type_tokens: TokenStream2,
}

struct CollectedFields {
    field_schemas: Vec<TokenStream2>,
    from_row_inits: Vec<TokenStream2>,
    /// Aliased counterparts of `from_row_inits` — read columns via
    /// `format!("{prefix}__{col}")` aliases so a Model can be
    /// decoded from a JOINed row's projected target columns.
    from_aliased_row_inits: Vec<TokenStream2>,
    /// Static column-name list — used by the simple insert path
    /// (no `Auto<T>` fields). Aligned with `insert_values`.
    insert_columns: Vec<TokenStream2>,
    /// Static `Into<SqlValue>` expressions, one per field. Aligned
    /// with `insert_columns`. Used by the simple insert path only.
    insert_values: Vec<TokenStream2>,
    /// Per-field push expressions for the dynamic (Auto-aware)
    /// insert path. Each statement either unconditionally pushes
    /// `(column, value)` or, for an `Auto<T>` field, conditionally
    /// pushes only when `Auto::Set(_)`. Built only when `has_auto`.
    insert_pushes: Vec<TokenStream2>,
    /// SQL columns for `RETURNING` — one per `Auto<T>` field. Empty
    /// when `has_auto == false`.
    returning_cols: Vec<TokenStream2>,
    /// `self.<field> = Row::try_get(&row, "<col>")?;` for each Auto
    /// field. Run after `insert_returning` to populate the model.
    auto_assigns: Vec<TokenStream2>,
    /// `(ident, column_literal)` pairs for every Auto field. Used by
    /// the bulk_insert codegen to rebuild assigns against `_row_mut`
    /// instead of `self`.
    auto_field_idents: Vec<(syn::Ident, String)>,
    /// Inner `T` of the first `Auto<T>` field, for the MySQL
    /// `LAST_INSERT_ID()` assignment in `AssignAutoPkPool`.
    first_auto_value_ty: Option<Type>,
    /// Bulk-insert per-row pushes for **non-Auto fields only**. Used
    /// by the all-Auto-Unset bulk path (Auto cols dropped from
    /// `columns`).
    bulk_pushes_no_auto: Vec<TokenStream2>,
    /// Bulk-insert per-row pushes for **all fields including Auto**.
    /// Used by the all-Auto-Set bulk path (Auto col included with the
    /// caller-supplied value).
    bulk_pushes_all: Vec<TokenStream2>,
    /// Column-name literals for non-Auto fields only (paired with
    /// `bulk_pushes_no_auto`).
    bulk_columns_no_auto: Vec<TokenStream2>,
    /// Column-name literals for every field including Auto (paired
    /// with `bulk_pushes_all`).
    bulk_columns_all: Vec<TokenStream2>,
    /// `let _i_unset_<n> = matches!(rows[0].<auto_field>, Auto::Unset);`
    /// + the loop that asserts every row matches. One pair per Auto
    /// field. Empty when `has_auto == false`.
    bulk_auto_uniformity: Vec<TokenStream2>,
    /// Identifier of the first Auto field, used as the witness for
    /// "all rows agree on Set vs Unset". Set only when `has_auto`.
    first_auto_ident: Option<syn::Ident>,
    /// `true` if any field on the struct is `Auto<T>`.
    has_auto: bool,
    /// `true` when the primary-key field's Rust type is `Auto<T>`.
    /// Gates `save()` codegen — only Auto PKs let us infer
    /// insert-vs-update from the in-memory value.
    pk_is_auto: bool,
    /// `Assignment` constructors for every non-PK column. Drives the
    /// UPDATE branch of `save()`.
    update_assignments: Vec<TokenStream2>,
    /// Column name literals (`"col"`) for every non-PK, non-auto_now_add column.
    /// Drives the `ON CONFLICT ... DO UPDATE SET` clause in `upsert_on`.
    upsert_update_columns: Vec<TokenStream2>,
    primary_key: Option<(syn::Ident, String)>,
    column_entries: Vec<ColumnEntry>,
    /// Rust-side field names, in declaration order. Used to validate
    /// container attributes like `display = "…"`.
    field_names: Vec<String>,
    /// FK fields on this child model. Drives the reverse-relation
    /// helper emit — for each FK, the macro adds an inherent
    /// `<parent>::<child_table>_set(&self, executor) -> Vec<Self>`
    /// method on the parent type.
    fk_relations: Vec<FkRelation>,
    /// SQL column name of the `#[rustango(soft_delete)]` field, if
    /// the model has one. Drives emission of the `soft_delete_on` /
    /// `restore_on` inherent methods. At most one such column per
    /// model is allowed; collect_fields rejects duplicates.
    soft_delete_column: Option<String>,
}

#[derive(Clone)]
struct FkRelation {
    /// Inner type of `ForeignKey<T, K>` — the parent model. The reverse
    /// helper is emitted as `impl <ParentType> { … }`.
    parent_type: Type,
    /// SQL column name on the child table for this FK (e.g. `"author"`).
    /// Used in the generated `WHERE <fk_column> = $1` clause.
    fk_column: String,
    /// `K`'s underlying scalar kind — drives the `match SqlValue { … }`
    /// arm emitted by [`load_related_impl_tokens`]. `I64` for the
    /// default `ForeignKey<T>` (no explicit K); other kinds when the
    /// user wrote `ForeignKey<T, String>`, `ForeignKey<T, Uuid>`, etc.
    pk_kind: DetectedKind,
    /// `true` when the field is `Option<ForeignKey<T, K>>` (nullable
    /// FK column). Drives the `Some(...)` wrapping in load_related
    /// assignment and `.as_ref().map(...)` in the FK PK accessor so
    /// the codegen matches the field's declared shape.
    nullable: bool,
}

fn collect_fields(named: &syn::FieldsNamed, table: &str) -> syn::Result<CollectedFields> {
    let cap = named.named.len();
    let mut out = CollectedFields {
        field_schemas: Vec::with_capacity(cap),
        from_row_inits: Vec::with_capacity(cap),
        from_aliased_row_inits: Vec::with_capacity(cap),
        insert_columns: Vec::with_capacity(cap),
        insert_values: Vec::with_capacity(cap),
        insert_pushes: Vec::with_capacity(cap),
        returning_cols: Vec::new(),
        auto_assigns: Vec::new(),
        auto_field_idents: Vec::new(),
        first_auto_value_ty: None,
        bulk_pushes_no_auto: Vec::with_capacity(cap),
        bulk_pushes_all: Vec::with_capacity(cap),
        bulk_columns_no_auto: Vec::with_capacity(cap),
        bulk_columns_all: Vec::with_capacity(cap),
        bulk_auto_uniformity: Vec::new(),
        first_auto_ident: None,
        has_auto: false,
        pk_is_auto: false,
        update_assignments: Vec::with_capacity(cap),
        upsert_update_columns: Vec::with_capacity(cap),
        primary_key: None,
        column_entries: Vec::with_capacity(cap),
        field_names: Vec::with_capacity(cap),
        fk_relations: Vec::new(),
        soft_delete_column: None,
    };

    for field in &named.named {
        let info = process_field(field, table)?;
        out.field_names.push(info.ident.to_string());
        out.field_schemas.push(info.schema);
        out.from_row_inits.push(info.from_row_init);
        out.from_aliased_row_inits.push(info.from_aliased_row_init);
        if let Some(parent_ty) = info.fk_inner.clone() {
            out.fk_relations.push(FkRelation {
                parent_type: parent_ty,
                fk_column: info.column.clone(),
                pk_kind: info.fk_pk_kind,
                nullable: info.nullable,
            });
        }
        if info.soft_delete {
            if out.soft_delete_column.is_some() {
                return Err(syn::Error::new_spanned(
                    field,
                    "only one field may be marked `#[rustango(soft_delete)]`",
                ));
            }
            out.soft_delete_column = Some(info.column.clone());
        }
        let column = info.column.as_str();
        let ident = info.ident;
        // Generated columns (`#[rustango(generated_as = "EXPR")]`)
        // skip every write path — Postgres recomputes the value
        // from EXPR. Push only the column-entry record (so typed
        // column constants still exist for filtering / projection)
        // and the schema literal (already pushed above) and move
        // on. No insert_columns/values, no insert_pushes, no
        // bulk_*, no update_assignments, no upsert_update_columns,
        // no returning_cols.
        if info.generated_as.is_some() {
            out.column_entries.push(ColumnEntry {
                ident: ident.clone(),
                value_ty: info.value_ty.clone(),
                name: ident.to_string(),
                column: info.column.clone(),
                field_type_tokens: info.field_type_tokens,
            });
            continue;
        }
        out.insert_columns.push(quote!(#column));
        out.insert_values.push(quote! {
            ::core::convert::Into::<::rustango::core::SqlValue>::into(
                ::core::clone::Clone::clone(&self.#ident)
            )
        });
        if info.auto {
            out.has_auto = true;
            if out.first_auto_ident.is_none() {
                out.first_auto_ident = Some(ident.clone());
                out.first_auto_value_ty = auto_inner_type(info.value_ty).cloned();
            }
            out.returning_cols.push(quote!(#column));
            out.auto_field_idents
                .push((ident.clone(), info.column.clone()));
            out.auto_assigns.push(quote! {
                self.#ident = ::rustango::sql::try_get_returning(_returning_row, #column)?;
            });
            out.insert_pushes.push(quote! {
                if let ::rustango::sql::Auto::Set(_v) = &self.#ident {
                    _columns.push(#column);
                    _values.push(::core::convert::Into::<::rustango::core::SqlValue>::into(
                        ::core::clone::Clone::clone(_v)
                    ));
                }
            });
            // Bulk: Auto fields appear only in the all-Set path,
            // never in the Unset path (we drop them from `columns`).
            out.bulk_columns_all.push(quote!(#column));
            out.bulk_pushes_all.push(quote! {
                _row_vals.push(::core::convert::Into::<::rustango::core::SqlValue>::into(
                    ::core::clone::Clone::clone(&_row.#ident)
                ));
            });
            // Uniformity check: every row's Auto state must match the
            // first row's. Mixed Set/Unset within one bulk_insert is
            // rejected here so the column list stays consistent.
            let ident_clone = ident.clone();
            out.bulk_auto_uniformity.push(quote! {
                for _r in rows.iter().skip(1) {
                    if matches!(_r.#ident_clone, ::rustango::sql::Auto::Unset) != _first_unset {
                        return ::core::result::Result::Err(
                            ::rustango::sql::ExecError::Sql(
                                ::rustango::sql::SqlError::BulkAutoMixed
                            )
                        );
                    }
                }
            });
        } else {
            out.insert_pushes.push(quote! {
                _columns.push(#column);
                _values.push(::core::convert::Into::<::rustango::core::SqlValue>::into(
                    ::core::clone::Clone::clone(&self.#ident)
                ));
            });
            // Bulk: non-Auto fields appear in BOTH paths.
            out.bulk_columns_no_auto.push(quote!(#column));
            out.bulk_columns_all.push(quote!(#column));
            let push_expr = quote! {
                _row_vals.push(::core::convert::Into::<::rustango::core::SqlValue>::into(
                    ::core::clone::Clone::clone(&_row.#ident)
                ));
            };
            out.bulk_pushes_no_auto.push(push_expr.clone());
            out.bulk_pushes_all.push(push_expr);
        }
        if info.primary_key {
            if out.primary_key.is_some() {
                return Err(syn::Error::new_spanned(
                    field,
                    "only one field may be marked `#[rustango(primary_key)]`",
                ));
            }
            out.primary_key = Some((ident.clone(), info.column.clone()));
            if info.auto {
                out.pk_is_auto = true;
            }
        } else if info.auto_now_add {
            // Immutable post-insert: skip from UPDATE entirely.
        } else if info.auto_now {
            // `auto_now` columns: bind `chrono::Utc::now()` on every
            // UPDATE so the column is always overridden with the
            // wall-clock at write time, regardless of what value the
            // user left in the struct field.
            out.update_assignments.push(quote! {
                ::rustango::core::Assignment {
                    column: #column,
                    value: ::core::convert::Into::<::rustango::core::Expr>::into(
                        ::core::convert::Into::<::rustango::core::SqlValue>::into(
                            ::chrono::Utc::now()
                        )
                    ),
                }
            });
            out.upsert_update_columns.push(quote!(#column));
        } else {
            out.update_assignments.push(quote! {
                ::rustango::core::Assignment {
                    column: #column,
                    value: ::core::convert::Into::<::rustango::core::Expr>::into(
                        ::core::convert::Into::<::rustango::core::SqlValue>::into(
                            ::core::clone::Clone::clone(&self.#ident)
                        )
                    ),
                }
            });
            out.upsert_update_columns.push(quote!(#column));
        }
        out.column_entries.push(ColumnEntry {
            ident: ident.clone(),
            value_ty: info.value_ty.clone(),
            name: ident.to_string(),
            column: info.column.clone(),
            field_type_tokens: info.field_type_tokens,
        });
    }
    Ok(out)
}

fn model_impl_tokens(
    struct_name: &syn::Ident,
    model_name: &str,
    table: &str,
    display: Option<&str>,
    app_label: Option<&str>,
    admin: Option<&AdminAttrs>,
    default_order: &[(String, bool, proc_macro2::Span)],
    field_schemas: &[TokenStream2],
    soft_delete_column: Option<&str>,
    permissions: bool,
    audit_track: Option<&[String]>,
    m2m_relations: &[M2MAttr],
    indexes: &[IndexAttr],
    checks: &[CheckAttr],
    composite_fks: &[CompositeFkAttr],
    generic_fks: &[GenericFkAttr],
    scope: Option<&str>,
    is_view: bool,
    verbose_name: Option<&str>,
    verbose_name_plural: Option<&str>,
) -> TokenStream2 {
    let display_tokens = if let Some(name) = display {
        quote!(::core::option::Option::Some(#name))
    } else {
        quote!(::core::option::Option::None)
    };
    let app_label_tokens = if let Some(name) = app_label {
        quote!(::core::option::Option::Some(#name))
    } else {
        quote!(::core::option::Option::None)
    };
    let soft_delete_tokens = if let Some(col) = soft_delete_column {
        quote!(::core::option::Option::Some(#col))
    } else {
        quote!(::core::option::Option::None)
    };
    let audit_track_tokens = match audit_track {
        None => quote!(::core::option::Option::None),
        Some(names) => {
            let lits = names.iter().map(|n| n.as_str());
            quote!(::core::option::Option::Some(&[ #(#lits),* ]))
        }
    };
    let admin_tokens = admin_config_tokens(admin);
    // Default `tenant` so single-tenant projects (no `scope` attr
    // anywhere) keep the v0.24.x behavior. Container-attr parser
    // already validated the value is "registry" or "tenant".
    let scope_tokens = match scope.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("registry") => quote!(::rustango::core::ModelScope::Registry),
        _ => quote!(::rustango::core::ModelScope::Tenant),
    };
    let verbose_name_tokens = optional_str(verbose_name);
    let verbose_name_plural_tokens = optional_str(verbose_name_plural);
    let indexes_tokens = indexes.iter().map(|idx| {
        let name = idx.name.as_deref().unwrap_or("unnamed_index");
        let cols: Vec<&str> = idx.columns.iter().map(String::as_str).collect();
        let unique = idx.unique;
        // Map the parsed method string onto the IndexMethod enum
        // variant — kept at the codegen layer so the IR doesn't
        // carry the string form.
        let method_variant = match idx.method.as_str() {
            "gin" => quote!(::rustango::core::IndexMethod::Gin),
            "gist" => quote!(::rustango::core::IndexMethod::Gist),
            "brin" => quote!(::rustango::core::IndexMethod::Brin),
            "spgist" => quote!(::rustango::core::IndexMethod::SpGist),
            "hash" => quote!(::rustango::core::IndexMethod::Hash),
            "bloom" => quote!(::rustango::core::IndexMethod::Bloom),
            _ => quote!(::rustango::core::IndexMethod::BTree),
        };
        let where_clause = match &idx.where_clause {
            Some(s) => quote!(::core::option::Option::Some(#s)),
            None => quote!(::core::option::Option::None),
        };
        quote! {
            ::rustango::core::IndexSchema {
                name: #name,
                columns: &[ #(#cols),* ],
                unique: #unique,
                method: #method_variant,
                where_clause: #where_clause,
            }
        }
    });
    let checks_tokens = checks.iter().map(|c| {
        let name = c.name.as_str();
        let expr = c.expr.as_str();
        quote! {
            ::rustango::core::CheckConstraint {
                name: #name,
                expr: #expr,
            }
        }
    });
    let composite_fk_tokens = composite_fks.iter().map(|rel| {
        let name = rel.name.as_str();
        let to = rel.to.as_str();
        let from_cols: Vec<&str> = rel.from.iter().map(String::as_str).collect();
        let on_cols: Vec<&str> = rel.on.iter().map(String::as_str).collect();
        quote! {
            ::rustango::core::CompositeFkRelation {
                name: #name,
                to: #to,
                from: &[ #(#from_cols),* ],
                on: &[ #(#on_cols),* ],
            }
        }
    });
    let generic_fk_tokens = generic_fks.iter().map(|rel| {
        let name = rel.name.as_str();
        let ct_col = rel.ct_column.as_str();
        let pk_col = rel.pk_column.as_str();
        quote! {
            ::rustango::core::GenericRelation {
                name: #name,
                ct_column: #ct_col,
                pk_column: #pk_col,
            }
        }
    });
    // Issue #291 / T2.5 — `default_order` slice literal. Empty when
    // no `#[rustango(default_order = "...")]` attribute was supplied.
    let default_order_tokens = default_order.iter().map(|(col, desc, _)| {
        let col_lit = col.as_str();
        quote! { (#col_lit, #desc) }
    });

    let m2m_tokens = m2m_relations.iter().map(|rel| {
        let name = rel.name.as_str();
        let to = rel.to.as_str();
        let through = rel.through.as_str();
        let src = rel.src.as_str();
        let dst = rel.dst.as_str();
        quote! {
            ::rustango::core::M2MRelation {
                name: #name,
                to: #to,
                through: #through,
                src_col: #src,
                dst_col: #dst,
            }
        }
    });
    quote! {
        impl ::rustango::core::Model for #struct_name {
            const SCHEMA: &'static ::rustango::core::ModelSchema = &::rustango::core::ModelSchema {
                name: #model_name,
                table: #table,
                fields: &[ #(#field_schemas),* ],
                display: #display_tokens,
                app_label: #app_label_tokens,
                admin: #admin_tokens,
                soft_delete_column: #soft_delete_tokens,
                permissions: #permissions,
                audit_track: #audit_track_tokens,
                m2m: &[ #(#m2m_tokens),* ],
                indexes: &[ #(#indexes_tokens),* ],
                check_constraints: &[ #(#checks_tokens),* ],
                composite_relations: &[ #(#composite_fk_tokens),* ],
                generic_relations: &[ #(#generic_fk_tokens),* ],
                scope: #scope_tokens,
                default_order: &[ #(#default_order_tokens),* ],
                is_view: #is_view,
                verbose_name: #verbose_name_tokens,
                verbose_name_plural: #verbose_name_plural_tokens,
            };
        }
    }
}

/// Emit the `admin: Option<&'static AdminConfig>` field for the model
/// schema. `None` when the user wrote no `#[rustango(admin(...))]`;
/// otherwise a static reference to a populated `AdminConfig`.
fn admin_config_tokens(admin: Option<&AdminAttrs>) -> TokenStream2 {
    let Some(admin) = admin else {
        return quote!(::core::option::Option::None);
    };

    let list_display = admin
        .list_display
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let list_display_lits = list_display.iter().map(|s| s.as_str());

    let search_fields = admin
        .search_fields
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let search_fields_lits = search_fields.iter().map(|s| s.as_str());

    let readonly_fields = admin
        .readonly_fields
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let readonly_fields_lits = readonly_fields.iter().map(|s| s.as_str());

    let list_filter = admin
        .list_filter
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let list_filter_lits = list_filter.iter().map(|s| s.as_str());

    let actions = admin
        .actions
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let actions_lits = actions.iter().map(|s| s.as_str());

    let fieldsets = admin
        .fieldsets
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let fieldset_tokens = fieldsets.iter().map(|(title, fields)| {
        let title = title.as_str();
        let field_lits = fields.iter().map(|s| s.as_str());
        quote!(::rustango::core::Fieldset {
            title: #title,
            fields: &[ #( #field_lits ),* ],
        })
    });

    let list_per_page = admin.list_per_page.unwrap_or(0);

    let ordering_pairs = admin
        .ordering
        .as_ref()
        .map(|(v, _)| v.as_slice())
        .unwrap_or(&[]);
    let ordering_tokens = ordering_pairs.iter().map(|(name, desc)| {
        let name = name.as_str();
        let desc = *desc;
        quote!((#name, #desc))
    });

    quote! {
        ::core::option::Option::Some(&::rustango::core::AdminConfig {
            list_display: &[ #( #list_display_lits ),* ],
            search_fields: &[ #( #search_fields_lits ),* ],
            list_per_page: #list_per_page,
            ordering: &[ #( #ordering_tokens ),* ],
            readonly_fields: &[ #( #readonly_fields_lits ),* ],
            list_filter: &[ #( #list_filter_lits ),* ],
            actions: &[ #( #actions_lits ),* ],
            fieldsets: &[ #( #fieldset_tokens ),* ],
        })
    }
}

fn inherent_impl_tokens(
    struct_name: &syn::Ident,
    fields: &CollectedFields,
    primary_key: Option<&(syn::Ident, String)>,
    column_consts: &TokenStream2,
    audited_fields: Option<&[&ColumnEntry]>,
    indexes: &[IndexAttr],
    manager_fns: &[syn::Ident],
) -> TokenStream2 {
    // Audit-emit fragments threaded into write paths. Non-empty only
    // when the model carries `#[rustango(audit(...))]`. They reborrow
    // `_executor` (a `&mut PgConnection` for audited models — the
    // macro switches the signature below) so the data write and the
    // audit INSERT both run on the same caller-supplied connection.
    let executor_passes_to_data_write = if audited_fields.is_some() {
        quote!(&mut *_executor)
    } else {
        quote!(_executor)
    };
    let executor_param = if audited_fields.is_some() {
        quote!(_executor: &mut ::rustango::sql::sqlx::PgConnection)
    } else {
        quote!(_executor: _E)
    };
    let executor_generics = if audited_fields.is_some() {
        quote!()
    } else {
        quote!(<'_c, _E>)
    };
    let executor_where = if audited_fields.is_some() {
        quote!()
    } else {
        quote! {
            where
                _E: ::rustango::sql::sqlx::Executor<'_c, Database = ::rustango::sql::sqlx::Postgres>,
        }
    };
    // For audited models the `_on` methods take `&mut PgConnection`, so
    // the &PgPool convenience wrappers (`save`, `insert`, `delete`)
    // must acquire a connection first. Non-audited models keep the
    // direct delegation since `&PgPool` IS an Executor.
    let pool_to_save_on = if audited_fields.is_some() {
        quote! {
            let mut _conn = pool.acquire().await?;
            self.save_on(&mut *_conn).await
        }
    } else {
        quote!(self.save_on(pool).await)
    };
    let pool_to_insert_on = if audited_fields.is_some() {
        quote! {
            let mut _conn = pool.acquire().await?;
            self.insert_on(&mut *_conn).await
        }
    } else {
        quote!(self.insert_on(pool).await)
    };
    let pool_to_delete_on = if audited_fields.is_some() {
        quote! {
            let mut _conn = pool.acquire().await?;
            self.delete_on(&mut *_conn).await
        }
    } else {
        quote!(self.delete_on(pool).await)
    };
    let pool_to_bulk_insert_on = if audited_fields.is_some() {
        quote! {
            let mut _conn = pool.acquire().await?;
            Self::bulk_insert_on(rows, &mut *_conn).await
        }
    } else {
        quote!(Self::bulk_insert_on(rows, pool).await)
    };
    // Pre-existing bug surfaced by batch 22's first audited Auto<T>
    // PK test model: `upsert(&PgPool)` body called `self.upsert_on(pool)`
    // directly, but `upsert_on` for audited models takes
    // `&mut PgConnection` (the audit emit needs a real connection).
    // Add the missing acquire shim to keep audited Auto-PK upsert
    // compiling.
    let pool_to_upsert_on = if audited_fields.is_some() {
        quote! {
            let mut _conn = pool.acquire().await?;
            self.upsert_on(&mut *_conn).await
        }
    } else {
        quote!(self.upsert_on(pool).await)
    };

    // `insert_pool(&Pool)` — v0.23.0-batch9. Non-audited models only
    // (audit-on-connection over &Pool needs a bi-dialect transaction
    // helper, deferred). Two body shapes:
    // - has_auto: build InsertQuery skipping Auto::Unset columns,
    //   request Auto cols in `returning`, dispatch via
    //   `insert_returning_pool`, then on the returned `PgRow` /
    //   `MySqlAutoId(id)` enum — pull each Auto field from the PG
    //   row OR drop the single i64 into the first Auto field on MySQL
    //   (multi-Auto models on MySQL error at runtime since
    //   `LAST_INSERT_ID()` only reports one)
    // - non-Auto: build InsertQuery with explicit columns/values and
    //   call `insert_pool` (no returning needed)
    // pool_insert_method body for the audited Auto-PK case is moved
    // to after audit_pair_tokens / audit_pk_to_string (they live
    // ~150 lines below). This block keeps the non-audited and
    // non-Auto branches in place — the audited Auto-PK arm is
    // computed below and merged via the dispatch helper variable.
    let pool_insert_method = if audited_fields.is_some() && !fields.has_auto {
        // Audited models with explicit (non-Auto) PKs go through
        // the non-Auto insert path below — the audit emit is one
        // round-trip after the INSERT inside the same tx via
        // audit::save_one_with_audit? No, INSERT semantics
        // differ. For non-Auto PK + audited, route through a
        // dedicated insert + audit emit on the same tx, but defer
        // the macro emission to the audit-bundle-aware block below
        // — this `quote!()` placeholder gets overwritten there.
        quote!()
    } else if audited_fields.is_some() && fields.has_auto {
        // Audited Auto-PK insert_pool — assembled after the audit
        // bundles. Placeholder; real emission below.
        quote!()
    } else if fields.has_auto {
        let pushes = &fields.insert_pushes;
        let returning_cols = &fields.returning_cols;
        quote! {
            /// Insert this row against either backend, populating any
            /// `Auto<T>` PK from the auto-assigned value.
            ///
            /// # Errors
            /// As [`Self::insert`].
            pub async fn insert_pool(
                &mut self,
                pool: &::rustango::sql::Pool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                let mut _columns: ::std::vec::Vec<&'static str> =
                    ::std::vec::Vec::new();
                let mut _values: ::std::vec::Vec<::rustango::core::SqlValue> =
                    ::std::vec::Vec::new();
                #( #pushes )*
                let _query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: _columns,
                    values: _values,
                    returning: ::std::vec![ #( #returning_cols ),* ],
                    on_conflict: ::core::option::Option::None,
                };
                let _result = ::rustango::sql::insert_returning_pool(
                    pool, &_query,
                ).await?;
                ::rustango::sql::apply_auto_pk(_result, self)
            }
        }
    } else {
        let insert_columns = &fields.insert_columns;
        let insert_values = &fields.insert_values;
        quote! {
            /// Insert this row into its table against either backend.
            /// Equivalent to [`Self::insert`] but takes
            /// [`::rustango::sql::Pool`].
            ///
            /// # Errors
            /// As [`Self::insert`].
            pub async fn insert_pool(
                &self,
                pool: &::rustango::sql::Pool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                let _query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #insert_columns ),* ],
                    values: ::std::vec![ #( #insert_values ),* ],
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::None,
                };
                ::rustango::sql::insert_pool(pool, &_query).await
            }
        }
    };

    // pool_save_method moved to after audit_pair_tokens /
    // audit_pk_to_string (they live ~70 lines below) — needed for
    // the audited branch which builds an UpdateQuery + PendingEntry
    // and dispatches via audit::save_one_with_audit.

    // pool_delete_method moved to after audit_pair_tokens / audit_pk_to_string
    // are computed (they live ~80 lines below).

    // Build the (column, JSON value) pair list used by every
    // snapshot-style audit emission. Reused across delete_on,
    // soft_delete_on, restore_on, and (later) bulk paths. Empty
    // when the model isn't audited.
    let audit_pair_tokens: Vec<TokenStream2> = audited_fields
        .map(|tracked| {
            tracked
                .iter()
                .map(|c| {
                    let column_lit = c.column.as_str();
                    let ident = &c.ident;
                    quote! {
                        (
                            #column_lit,
                            ::serde_json::to_value(&self.#ident)
                                .unwrap_or(::serde_json::Value::Null),
                        )
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let audit_pk_to_string = if let Some((pk_ident, _)) = primary_key {
        if fields.pk_is_auto {
            quote!(self.#pk_ident.get().map(|v| ::std::format!("{}", v)).unwrap_or_default())
        } else {
            quote!(::std::format!("{}", &self.#pk_ident))
        }
    } else {
        quote!(::std::string::String::new())
    };
    let make_op_emit = |op_path: TokenStream2| -> TokenStream2 {
        if audited_fields.is_some() {
            let pairs = audit_pair_tokens.iter();
            let pk_str = audit_pk_to_string.clone();
            quote! {
                let _audit_entry = ::rustango::audit::PendingEntry {
                    entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                    entity_pk: #pk_str,
                    operation: #op_path,
                    source: ::rustango::audit::current_source(),
                    changes: ::rustango::audit::snapshot_changes(&[
                        #( #pairs ),*
                    ]),
                };
                ::rustango::audit::emit_one(&mut *_executor, &_audit_entry).await?;
            }
        } else {
            quote!()
        }
    };
    let audit_insert_emit = make_op_emit(quote!(::rustango::audit::AuditOp::Create));
    let audit_delete_emit = make_op_emit(quote!(::rustango::audit::AuditOp::Delete));
    let audit_softdelete_emit = make_op_emit(quote!(::rustango::audit::AuditOp::SoftDelete));
    let audit_restore_emit = make_op_emit(quote!(::rustango::audit::AuditOp::Restore));

    // `save_pool(&Pool)` — emitted for every model with a PK.
    // Audited Auto-PK models are deferred (the Auto::Unset →
    // insert_pool path needs the audited-insert flow from a future
    // batch). Three body shapes:
    // - non-audited, plain PK: build UpdateQuery + dispatch through
    //   sql::update_pool
    // - non-audited, Auto-PK: same, but Auto::Unset routes to
    //   self.insert_pool which already handles RETURNING / LAST_INSERT_ID
    // - audited, plain PK: build UpdateQuery + PendingEntry, dispatch
    //   through audit::save_one_with_audit (per-backend tx wraps
    //   UPDATE + audit emit atomically). Snapshot-style audit (post-
    //   write field values) — diff-style audit (with pre-UPDATE
    //   SELECT for `before` values) needs per-tracked-column codegen
    //   that doesn't fit the runtime-helper pattern; legacy &PgPool
    //   `save` keeps the diff for now.
    let pool_save_method = if let Some((pk_ident, pk_col)) = primary_key {
        let pk_column_lit = pk_col.as_str();
        let assignments = &fields.update_assignments;
        if audited_fields.is_some() {
            if fields.pk_is_auto {
                // Auto-PK + audited: defer. The Auto::Unset insert
                // path needs a transactional INSERT + LAST_INSERT_ID
                // + audit emit flow — that's a follow-up batch.
                quote!()
            } else {
                let pairs = audit_pair_tokens.iter();
                let pairs2 = audit_pair_tokens.iter();
                let pk_str = audit_pk_to_string.clone();
                let pk_str2 = audit_pk_to_string.clone();
                quote! {
                    /// Save (UPDATE) this row against either backend
                    /// with audit emission inside the same transaction.
                    /// Bi-dialect counterpart of [`Self::save`] for
                    /// audited models with non-`Auto<T>` PKs.
                    ///
                    /// Captures **post-write** field state (snapshot
                    /// audit). The legacy &PgPool [`Self::save`]
                    /// captures BEFORE+AFTER for true diff audit;
                    /// porting that to the &Pool path needs runtime
                    /// per-tracked-column decoding and is deferred.
                    ///
                    /// # Errors
                    /// As [`Self::save`].
                    pub async fn save_pool(
                        &mut self,
                        pool: &::rustango::sql::Pool,
                    ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                        let _query = ::rustango::core::UpdateQuery {
                            model: <Self as ::rustango::core::Model>::SCHEMA,
                            set: ::std::vec![ #( #assignments ),* ],
                            where_clause: ::rustango::core::WhereExpr::Predicate(
                                ::rustango::core::Filter {
                                    column: #pk_column_lit,
                                    op: ::rustango::core::Op::Eq,
                                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                        ::core::clone::Clone::clone(&self.#pk_ident)
                                    ),
                                }
                            ),
                        };
                        let _audit_entry = ::rustango::audit::PendingEntry {
                            entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                            entity_pk: #pk_str,
                            operation: ::rustango::audit::AuditOp::Update,
                            source: ::rustango::audit::current_source(),
                            changes: ::rustango::audit::snapshot_changes(&[
                                #( #pairs ),*
                            ]),
                        };
                        let _ = ::rustango::audit::save_one_with_audit(
                            pool, &_query, &_audit_entry,
                        ).await?;
                        ::core::result::Result::Ok(())
                    }

                    /// `save_pool` narrowed to a Rust-field allowlist — issue #66
                    /// (Django `Model.save(update_fields=[...])`).
                    /// Audit emission shrinks to the same column set so
                    /// the audit log reflects exactly what was written.
                    ///
                    /// # Errors
                    /// As [`Self::save_pool`], plus
                    /// [`::rustango::core::QueryError::UnknownField`] wrapped
                    /// in `ExecError::Query` for unknown field names.
                    pub async fn save_partial(
                        &mut self,
                        fields: &[&str],
                        pool: &::rustango::sql::Pool,
                    ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                        if fields.is_empty() {
                            ::tracing::warn!(
                                target: "rustango::save_partial",
                                model = <Self as ::rustango::core::Model>::SCHEMA.name,
                                "save_partial called with empty field list — no-op"
                            );
                            return ::core::result::Result::Ok(());
                        }
                        let _schema = <Self as ::rustango::core::Model>::SCHEMA;
                        let mut _wanted_cols: ::std::collections::HashSet<&'static str> =
                            ::std::collections::HashSet::with_capacity(fields.len());
                        for f in fields {
                            match _schema.field(f) {
                                ::core::option::Option::Some(fs) => {
                                    _wanted_cols.insert(fs.column);
                                }
                                ::core::option::Option::None => {
                                    return ::core::result::Result::Err(
                                        ::rustango::sql::ExecError::Query(
                                            ::rustango::core::QueryError::UnknownField {
                                                model: _schema.name,
                                                field: (*f).to_owned(),
                                            }
                                        )
                                    );
                                }
                            }
                        }
                        let _full: ::std::vec::Vec<::rustango::core::Assignment> =
                            ::std::vec![ #( #assignments ),* ];
                        let _filtered: ::std::vec::Vec<::rustango::core::Assignment> = _full
                            .into_iter()
                            .filter(|a| _wanted_cols.contains(a.column))
                            .collect();
                        if _filtered.is_empty() {
                            ::tracing::warn!(
                                target: "rustango::save_partial",
                                model = _schema.name,
                                "save_partial: every named field maps to a non-assignable column — no-op"
                            );
                            return ::core::result::Result::Ok(());
                        }
                        let _query = ::rustango::core::UpdateQuery {
                            model: _schema,
                            set: _filtered,
                            where_clause: ::rustango::core::WhereExpr::Predicate(
                                ::rustango::core::Filter {
                                    column: #pk_column_lit,
                                    op: ::rustango::core::Op::Eq,
                                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                        ::core::clone::Clone::clone(&self.#pk_ident)
                                    ),
                                }
                            ),
                        };
                        // Narrow the audit snapshot to the same column set.
                        let _all_pairs: ::std::vec::Vec<(&'static str, ::serde_json::Value)> =
                            ::std::vec![ #( #pairs2 ),* ];
                        let _narrowed: ::std::vec::Vec<(&'static str, ::serde_json::Value)> =
                            _all_pairs
                                .into_iter()
                                .filter(|(col, _)| _wanted_cols.contains(col))
                                .collect();
                        let _audit_entry = ::rustango::audit::PendingEntry {
                            entity_table: _schema.table,
                            entity_pk: #pk_str2,
                            operation: ::rustango::audit::AuditOp::Update,
                            source: ::rustango::audit::current_source(),
                            changes: ::rustango::audit::snapshot_changes(&_narrowed),
                        };
                        let _ = ::rustango::audit::save_one_with_audit(
                            pool, &_query, &_audit_entry,
                        ).await?;
                        ::core::result::Result::Ok(())
                    }

                    /// Typed-column counterpart of [`Self::save_partial`] —
                    /// issue #67. `fields` is a tuple of [`Column`]
                    /// constants whose `Model` matches `Self`; typos and
                    /// model mismatches surface at *compile time*
                    /// (`Author::name` inside a `Post::save_partial_typed`
                    /// call is a type error, no runtime check).
                    ///
                    /// ```ignore
                    /// post.save_partial_typed((Post::title, Post::slug), &pool).await?;
                    /// ```
                    ///
                    /// Lowers to [`Self::save_partial`] under the hood;
                    /// audit narrowing + every other semantic is identical.
                    ///
                    /// [`Column`]: ::rustango::core::Column
                    ///
                    /// # Errors
                    /// As [`Self::save_partial`].
                    pub async fn save_partial_typed<
                        L: ::rustango::core::TypedFieldList<Self>,
                    >(
                        &mut self,
                        fields: L,
                        pool: &::rustango::sql::Pool,
                    ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                        let _names = fields.rust_field_names();
                        let _refs: ::std::vec::Vec<&str> =
                            _names.iter().copied().collect();
                        self.save_partial(&_refs, pool).await
                    }
                }
            }
        } else {
            let dispatch_unset = if fields.pk_is_auto {
                quote! {
                    if matches!(self.#pk_ident, ::rustango::sql::Auto::Unset) {
                        return self.insert_pool(pool).await;
                    }
                }
            } else {
                quote!()
            };
            quote! {
                /// Save this row to its table against either backend.
                /// `INSERT` when the `Auto<T>` PK is `Unset`, else
                /// `UPDATE` keyed on the PK.
                ///
                /// # Errors
                /// As [`Self::save`].
                pub async fn save_pool(
                    &mut self,
                    pool: &::rustango::sql::Pool,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    #dispatch_unset
                    let _query = ::rustango::core::UpdateQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        set: ::std::vec![ #( #assignments ),* ],
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    let _ = ::rustango::sql::update_pool(pool, &_query).await?;
                    ::core::result::Result::Ok(())
                }

                /// Save (UPDATE) only the listed Rust-side fields,
                /// leaving every other column untouched. Issue #66 —
                /// Django's `Model.save(update_fields=[...])` shape.
                ///
                /// `fields` are Rust-side struct field names; the macro
                /// resolves each to its SQL column. Unknown field
                /// names return [`::rustango::core::QueryError::UnknownField`]
                /// wrapped in `ExecError::Query`. An empty list is a
                /// no-op (returns `Ok(())` and logs a `tracing::warn!`),
                /// matching Django's "nothing to do" semantic.
                ///
                /// Use this when:
                /// * you only mutated a couple of fields on a wide row
                ///   (avoid re-writing every column on every save), or
                /// * two writers diverged after their initial read and
                ///   you want to preserve the other writer's changes to
                ///   columns you didn't touch.
                ///
                /// Auto-PK models with an unset PK return
                /// [`::rustango::core::QueryError::UnknownField`] with
                /// field name `<pk>` — `save_partial` is an
                /// UPDATE-only path. Call [`Self::insert_pool`]
                /// (or [`Self::save_pool`] which dispatches based on
                /// PK state) for the INSERT case.
                ///
                /// # Errors
                /// As [`Self::save_pool`], plus `UnknownField` for
                /// unknown / empty / Auto-Unset cases.
                pub async fn save_partial(
                    &mut self,
                    fields: &[&str],
                    pool: &::rustango::sql::Pool,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    if fields.is_empty() {
                        ::tracing::warn!(
                            target: "rustango::save_partial",
                            model = <Self as ::rustango::core::Model>::SCHEMA.name,
                            "save_partial called with empty field list — no-op"
                        );
                        return ::core::result::Result::Ok(());
                    }
                    let _schema = <Self as ::rustango::core::Model>::SCHEMA;
                    // Validate field names against the schema.
                    let mut _wanted_cols: ::std::collections::HashSet<&'static str> =
                        ::std::collections::HashSet::with_capacity(fields.len());
                    for f in fields {
                        match _schema.field(f) {
                            ::core::option::Option::Some(fs) => {
                                _wanted_cols.insert(fs.column);
                            }
                            ::core::option::Option::None => {
                                return ::core::result::Result::Err(
                                    ::rustango::sql::ExecError::Query(
                                        ::rustango::core::QueryError::UnknownField {
                                            model: _schema.name,
                                            field: (*f).to_owned(),
                                        }
                                    )
                                );
                            }
                        }
                    }
                    // Build the full assignment vec, then keep only the
                    // assignments whose column is in `_wanted_cols`.
                    let _full: ::std::vec::Vec<::rustango::core::Assignment> =
                        ::std::vec![ #( #assignments ),* ];
                    let _filtered: ::std::vec::Vec<::rustango::core::Assignment> = _full
                        .into_iter()
                        .filter(|a| _wanted_cols.contains(a.column))
                        .collect();
                    if _filtered.is_empty() {
                        // All field names valid, but they all map to
                        // non-assignable slots (PK column, computed/
                        // virtual fields, relations without an
                        // assignment). Same no-op semantic as Django.
                        ::tracing::warn!(
                            target: "rustango::save_partial",
                            model = _schema.name,
                            "save_partial: every named field maps to a non-assignable column — no-op"
                        );
                        return ::core::result::Result::Ok(());
                    }
                    let _query = ::rustango::core::UpdateQuery {
                        model: _schema,
                        set: _filtered,
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    let _ = ::rustango::sql::update_pool(pool, &_query).await?;
                    ::core::result::Result::Ok(())
                }

                /// Typed-column counterpart of [`Self::save_partial`] —
                /// issue #67. `fields` is a tuple of [`Column`]
                /// constants whose `Model` matches `Self`; typos and
                /// model mismatches surface at *compile time*
                /// (`Author::name` inside a `Post::save_partial_typed`
                /// call is a type error, no runtime check).
                ///
                /// ```ignore
                /// post.save_partial_typed((Post::title, Post::slug), &pool).await?;
                /// ```
                ///
                /// Lowers to [`Self::save_partial`] under the hood — the
                /// tuple is reduced to a `&[&str]` slice of Rust-side
                /// field names and forwarded.
                ///
                /// [`Column`]: ::rustango::core::Column
                ///
                /// # Errors
                /// As [`Self::save_partial`].
                pub async fn save_partial_typed<
                    L: ::rustango::core::TypedFieldList<Self>,
                >(
                    &mut self,
                    fields: L,
                    pool: &::rustango::sql::Pool,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    let _names = fields.rust_field_names();
                    let _refs: ::std::vec::Vec<&str> =
                        _names.iter().copied().collect();
                    self.save_partial(&_refs, pool).await
                }
            }
        }
    } else {
        quote!()
    };

    // Audited `insert_pool` (overrides the placeholder set higher up
    // in the function). v0.23.0-batch22 — both Auto-PK and non-Auto-PK
    // audited models get insert_pool routing through
    // audit::insert_one_with_audit (per-backend tx wraps INSERT
    // + auto-PK readback + audit emit). Snapshot-style audit (the
    // PendingEntry's `changes` carries post-write field values).
    let pool_insert_method = if audited_fields.is_some() {
        if let Some(_) = primary_key {
            let pushes = if fields.has_auto {
                fields.insert_pushes.clone()
            } else {
                // For non-Auto-PK models, the macro normally builds
                // {columns, values} from fields.insert_columns +
                // fields.insert_values rather than insert_pushes.
                // Map those into the pushes shape.
                fields
                    .insert_columns
                    .iter()
                    .zip(&fields.insert_values)
                    .map(|(col, val)| {
                        quote! {
                            _columns.push(#col);
                            _values.push(#val);
                        }
                    })
                    .collect()
            };
            let returning_cols: Vec<proc_macro2::TokenStream> = if fields.has_auto {
                fields.returning_cols.clone()
            } else {
                // Non-Auto-PK: still need RETURNING something for the
                // audit helper's contract (it errors on empty
                // returning). Return the PK column so the audit row
                // can carry the assigned PK back. Some non-Auto PKs
                // are server-side-default (e.g. UUIDv4 default), so
                // RETURNING is genuinely useful.
                primary_key
                    .map(|(_, col)| {
                        let lit = col.as_str();
                        vec![quote!(#lit)]
                    })
                    .unwrap_or_default()
            };
            let pairs = audit_pair_tokens.iter();
            let pk_str = audit_pk_to_string.clone();
            quote! {
                /// Insert this row against either backend with audit
                /// emission inside the same transaction. Bi-dialect
                /// counterpart of [`Self::insert`] for audited models.
                ///
                /// Snapshot-style audit (post-write field values).
                ///
                /// # Errors
                /// As [`Self::insert`].
                pub async fn insert_pool(
                    &mut self,
                    pool: &::rustango::sql::Pool,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    let mut _columns: ::std::vec::Vec<&'static str> =
                        ::std::vec::Vec::new();
                    let mut _values: ::std::vec::Vec<::rustango::core::SqlValue> =
                        ::std::vec::Vec::new();
                    #( #pushes )*
                    let _query = ::rustango::core::InsertQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        columns: _columns,
                        values: _values,
                        returning: ::std::vec![ #( #returning_cols ),* ],
                        on_conflict: ::core::option::Option::None,
                    };
                    let _audit_entry = ::rustango::audit::PendingEntry {
                        entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                        entity_pk: #pk_str,
                        operation: ::rustango::audit::AuditOp::Create,
                        source: ::rustango::audit::current_source(),
                        changes: ::rustango::audit::snapshot_changes(&[
                            #( #pairs ),*
                        ]),
                    };
                    let _result = ::rustango::audit::insert_one_with_audit(
                        pool, &_query, &_audit_entry,
                    ).await?;
                    ::rustango::sql::apply_auto_pk(_result, self)
                }
            }
        } else {
            quote!()
        }
    } else {
        // Keep the non-audited pool_insert_method we built earlier.
        pool_insert_method
    };

    // Update audited save_pool: now that insert_pool is wired for
    // audited Auto-PK models, save_pool can dispatch Auto::Unset →
    // insert_pool. Non-audited save_pool already does this.
    // v0.23.0-batch25 — diff-style audit on the audited save_pool path.
    // Replaces the snapshot-only emission with a per-backend transaction
    // body that:
    //  1. SELECTs the tracked columns by PK (typed Row::try_get per
    //     column), capturing BEFORE values
    //  2. compiles the UPDATE via pool.dialect() and runs it on the tx
    //  3. builds AFTER pairs from &self
    //  4. diffs BEFORE/AFTER, emits one PendingEntry with
    //     AuditOp::Update + diff_changes(...) on the same tx connection
    //  5. commits
    //
    // Per-backend arms inline the SQL string + placeholder shape, then
    // share the `audit_before_pair_tokens` decoder block (Row::try_get
    // is polymorphic over Row type — the same tokens work against
    // PgRow and MySqlRow as long as the field's Rust type implements
    // both Decode<Postgres> and Decode<MySql>, which Auto<T> +
    // primitives + chrono/uuid/serde_json::Value all do).
    let pool_save_method = if let Some(tracked) = audited_fields {
        if let Some((pk_ident, pk_col)) = primary_key {
            let pk_column_lit = pk_col.as_str();
            // Two iterators — quote!'s `#(#var)*` consumes the
            // iterator, and we need to splice the same after-pairs
            // sequence into both per-backend arms.
            let after_pairs_pg = audit_pair_tokens.iter().collect::<Vec<_>>();
            let pk_str = audit_pk_to_string.clone();
            // Per-tracked-column BEFORE-pair token list. Each entry
            // is `(col_lit, try_get_returning<value_ty>(row, col_lit) → Json)`.
            // The Row alias resolves to PgRow / MySqlRow per call site,
            // so the same template generates both the PG and MySQL bodies.
            let mk_before_pairs =
                |getter: proc_macro2::TokenStream| -> Vec<proc_macro2::TokenStream> {
                    tracked
                        .iter()
                        .map(|c| {
                            let column_lit = c.column.as_str();
                            let value_ty = &c.value_ty;
                            quote! {
                                (
                                    #column_lit,
                                    match #getter::<#value_ty>(
                                        _audit_before_row, #column_lit,
                                    ) {
                                        ::core::result::Result::Ok(v) => {
                                            ::serde_json::to_value(&v)
                                                .unwrap_or(::serde_json::Value::Null)
                                        }
                                        ::core::result::Result::Err(_) => ::serde_json::Value::Null,
                                    },
                                )
                            }
                        })
                        .collect()
                };
            let before_pairs_pg: Vec<proc_macro2::TokenStream> =
                mk_before_pairs(quote!(::rustango::sql::try_get_returning));
            let before_pairs_my: Vec<proc_macro2::TokenStream> =
                mk_before_pairs(quote!(::rustango::sql::try_get_returning_my));
            let before_pairs_sqlite: Vec<proc_macro2::TokenStream> =
                mk_before_pairs(quote!(::rustango::sql::try_get_returning_sqlite));
            let pg_select_cols: String = tracked
                .iter()
                .map(|c| format!("\"{}\"", c.column.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ");
            let my_select_cols: String = tracked
                .iter()
                .map(|c| format!("`{}`", c.column.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            // SQLite uses double-quote identifier quoting (same as
            // Postgres in default config), so the column-list shape
            // matches PG.
            let sqlite_select_cols: String = pg_select_cols.clone();
            let pk_value_for_bind = if fields.pk_is_auto {
                quote!(self.#pk_ident.get().copied().unwrap_or_default())
            } else {
                quote!(::core::clone::Clone::clone(&self.#pk_ident))
            };
            let assignments = &fields.update_assignments;
            let unset_dispatch = if fields.has_auto {
                quote! {
                    if matches!(self.#pk_ident, ::rustango::sql::Auto::Unset) {
                        return self.insert_pool(pool).await;
                    }
                }
            } else {
                quote!()
            };
            quote! {
                /// Save this row against either backend with audit
                /// emission (diff-style: BEFORE+AFTER) inside the
                /// same transaction. Auto::Unset PK routes to
                /// insert_pool. Bi-dialect counterpart of
                /// [`Self::save`] for audited models.
                ///
                /// The audit row's `changes` JSON contains one
                /// `{ "field": { "before": …, "after": … } }` entry
                /// per tracked column whose value actually changed
                /// — same shape as the existing &PgPool save() emits.
                ///
                /// # Errors
                /// As [`Self::save`].
                pub async fn save_pool(
                    &mut self,
                    pool: &::rustango::sql::Pool,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    #unset_dispatch
                    let _query = ::rustango::core::UpdateQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        set: ::std::vec![ #( #assignments ),* ],
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    let _after_pairs: ::std::vec::Vec<(&'static str, ::serde_json::Value)> =
                        ::std::vec![ #( #after_pairs_pg ),* ];
                    ::rustango::audit::save_one_with_diff(
                        pool,
                        &_query,
                        #pk_column_lit,
                        ::core::convert::Into::<::rustango::core::SqlValue>::into(
                            #pk_value_for_bind,
                        ),
                        <Self as ::rustango::core::Model>::SCHEMA.table,
                        #pk_str,
                        _after_pairs,
                        #pg_select_cols,
                        #my_select_cols,
                        #sqlite_select_cols,
                        |_audit_before_row| ::std::vec![ #( #before_pairs_pg ),* ],
                        |_audit_before_row| ::std::vec![ #( #before_pairs_my ),* ],
                        |_audit_before_row| ::std::vec![ #( #before_pairs_sqlite ),* ],
                    ).await
                }
            }
        } else {
            quote!()
        }
    } else {
        pool_save_method
    };

    // `delete_pool(&Pool)` — emitted for every model with a PK. Two
    // body shapes:
    // - non-audited: simple dispatch through `sql::delete_pool`
    // - audited: routes through `audit::delete_one_with_audit`,
    //   which opens a per-backend transaction wrapping DELETE +
    //   audit emit so the data write and audit row commit atomically.
    let pool_delete_method = {
        let pk_column_lit = primary_key.map(|(_, col)| col.as_str()).unwrap_or("id");
        let pk_ident_for_pool = primary_key.map(|(ident, _)| ident);
        if let Some(pk_ident) = pk_ident_for_pool {
            if audited_fields.is_some() {
                let pairs = audit_pair_tokens.iter();
                let pk_str = audit_pk_to_string.clone();
                quote! {
                    /// Delete this row against either backend with audit
                    /// emission inside the same transaction. Bi-dialect
                    /// counterpart of [`Self::delete`] for audited models.
                    ///
                    /// # Errors
                    /// As [`Self::delete`].
                    pub async fn delete_pool(
                        &self,
                        pool: &::rustango::sql::Pool,
                    ) -> ::core::result::Result<u64, ::rustango::sql::ExecError> {
                        let _query = ::rustango::core::DeleteQuery {
                            model: <Self as ::rustango::core::Model>::SCHEMA,
                            where_clause: ::rustango::core::WhereExpr::Predicate(
                                ::rustango::core::Filter {
                                    column: #pk_column_lit,
                                    op: ::rustango::core::Op::Eq,
                                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                        ::core::clone::Clone::clone(&self.#pk_ident)
                                    ),
                                }
                            ),
                        };
                        let _audit_entry = ::rustango::audit::PendingEntry {
                            entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                            entity_pk: #pk_str,
                            operation: ::rustango::audit::AuditOp::Delete,
                            source: ::rustango::audit::current_source(),
                            changes: ::rustango::audit::snapshot_changes(&[
                                #( #pairs ),*
                            ]),
                        };
                        ::rustango::audit::delete_one_with_audit(
                            pool, &_query, &_audit_entry,
                        ).await
                    }
                }
            } else {
                quote! {
                    /// Delete the row identified by this instance's primary key
                    /// against either backend. Equivalent to [`Self::delete`] but
                    /// takes [`::rustango::sql::Pool`] and dispatches per backend.
                    ///
                    /// # Errors
                    /// As [`Self::delete`].
                    pub async fn delete_pool(
                        &self,
                        pool: &::rustango::sql::Pool,
                    ) -> ::core::result::Result<u64, ::rustango::sql::ExecError> {
                        let _query = ::rustango::core::DeleteQuery {
                            model: <Self as ::rustango::core::Model>::SCHEMA,
                            where_clause: ::rustango::core::WhereExpr::Predicate(
                                ::rustango::core::Filter {
                                    column: #pk_column_lit,
                                    op: ::rustango::core::Op::Eq,
                                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                        ::core::clone::Clone::clone(&self.#pk_ident)
                                    ),
                                }
                            ),
                        };
                        ::rustango::sql::delete_pool(pool, &_query).await
                    }
                }
            }
        } else {
            quote!()
        }
    };

    // `_tx` family — `insert_tx`, `save_tx`, `delete_tx`. These mirror
    // the non-audited `_pool` methods but execute against an open
    // `PoolTx` so the writes participate in the caller's transaction.
    // Auditing inside TX is deferred; these always use the plain
    // executor primitives regardless of whether the model is audited.
    let tx_insert_method = if fields.has_auto {
        let pushes = &fields.insert_pushes;
        let returning_cols = &fields.returning_cols;
        quote! {
            /// Insert this row inside an open transaction, populating
            /// any `Auto<T>` PK from the auto-assigned value. Works
            /// against any backend that `tx` wraps.
            ///
            /// # Errors
            /// As [`Self::insert_pool`].
            pub async fn insert_tx(
                &mut self,
                tx: &mut ::rustango::sql::PoolTx<'_>,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                let mut _columns: ::std::vec::Vec<&'static str> =
                    ::std::vec::Vec::new();
                let mut _values: ::std::vec::Vec<::rustango::core::SqlValue> =
                    ::std::vec::Vec::new();
                #( #pushes )*
                let _query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: _columns,
                    values: _values,
                    returning: ::std::vec![ #( #returning_cols ),* ],
                    on_conflict: ::core::option::Option::None,
                };
                let _result = ::rustango::sql::insert_returning_tx(tx, &_query).await?;
                ::rustango::sql::apply_auto_pk(_result, self)
            }
        }
    } else {
        let insert_columns = &fields.insert_columns;
        let insert_values = &fields.insert_values;
        quote! {
            /// Insert this row inside an open transaction.
            ///
            /// # Errors
            /// As [`Self::insert_pool`].
            pub async fn insert_tx(
                &self,
                tx: &mut ::rustango::sql::PoolTx<'_>,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                let _query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #insert_columns ),* ],
                    values: ::std::vec![ #( #insert_values ),* ],
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::None,
                };
                ::rustango::sql::insert_tx(tx, &_query).await
            }
        }
    };

    let tx_save_method = if let Some((pk_ident, pk_col)) = primary_key {
        let pk_column_lit = pk_col.as_str();
        let assignments = &fields.update_assignments;
        let dispatch_unset = if fields.pk_is_auto {
            quote! {
                if matches!(self.#pk_ident, ::rustango::sql::Auto::Unset) {
                    return self.insert_tx(tx).await;
                }
            }
        } else {
            quote!()
        };
        quote! {
            /// Save this row inside an open transaction. `INSERT` when
            /// the `Auto<T>` PK is `Unset`, else `UPDATE` keyed on the
            /// PK. Works against any backend that `tx` wraps.
            ///
            /// # Errors
            /// As [`Self::save_pool`].
            pub async fn save_tx(
                &mut self,
                tx: &mut ::rustango::sql::PoolTx<'_>,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                #dispatch_unset
                let _query = ::rustango::core::UpdateQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    set: ::std::vec![ #( #assignments ),* ],
                    where_clause: ::rustango::core::WhereExpr::Predicate(
                        ::rustango::core::Filter {
                            column: #pk_column_lit,
                            op: ::rustango::core::Op::Eq,
                            value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                ::core::clone::Clone::clone(&self.#pk_ident)
                            ),
                        }
                    ),
                };
                let _ = ::rustango::sql::update_tx(tx, &_query).await?;
                ::core::result::Result::Ok(())
            }
        }
    } else {
        quote!()
    };

    let tx_delete_method = {
        let pk_column_lit = primary_key.map(|(_, col)| col.as_str()).unwrap_or("id");
        let pk_ident_for_tx = primary_key.map(|(ident, _)| ident);
        if let Some(pk_ident) = pk_ident_for_tx {
            quote! {
                /// Delete the row identified by this instance's PK
                /// inside an open transaction. Works against any backend
                /// that `tx` wraps.
                ///
                /// # Errors
                /// As [`Self::delete_pool`].
                pub async fn delete_tx(
                    &self,
                    tx: &mut ::rustango::sql::PoolTx<'_>,
                ) -> ::core::result::Result<u64, ::rustango::sql::ExecError> {
                    let _query = ::rustango::core::DeleteQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    ::rustango::sql::delete_tx(tx, &_query).await
                }
            }
        } else {
            quote!()
        }
    };

    // Update emission captures both BEFORE and AFTER state — runs an
    // extra SELECT against `_executor` BEFORE the UPDATE, captures
    // each tracked field's prior value, then after the UPDATE diffs
    // against the in-memory `&self`. `diff_changes` drops unchanged
    // columns so the JSON only contains the actual delta.
    //
    // Two-fragment shape: `audit_update_pre` runs before the UPDATE
    // and binds `_audit_before_pairs`; `audit_update_post` runs
    // after the UPDATE and emits the PendingEntry.
    let (audit_update_pre, audit_update_post): (TokenStream2, TokenStream2) = if let Some(tracked) =
        audited_fields
    {
        if tracked.is_empty() {
            (quote!(), quote!())
        } else {
            let select_cols: String = tracked
                .iter()
                .map(|c| format!("\"{}\"", c.column.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ");
            let pk_column_for_select = primary_key.map(|(_, col)| col.clone()).unwrap_or_default();
            let select_cols_lit = select_cols;
            let pk_column_lit_for_select = pk_column_for_select;
            let pk_value_for_bind = if let Some((pk_ident, _)) = primary_key {
                if fields.pk_is_auto {
                    quote!(self.#pk_ident.get().copied().unwrap_or_default())
                } else {
                    quote!(::core::clone::Clone::clone(&self.#pk_ident))
                }
            } else {
                quote!(0_i64)
            };
            let before_pairs = tracked.iter().map(|c| {
                let column_lit = c.column.as_str();
                let value_ty = &c.value_ty;
                quote! {
                    (
                        #column_lit,
                        match ::rustango::sql::sqlx::Row::try_get::<#value_ty, _>(
                            &_audit_before_row, #column_lit,
                        ) {
                            ::core::result::Result::Ok(v) => {
                                ::serde_json::to_value(&v)
                                    .unwrap_or(::serde_json::Value::Null)
                            }
                            ::core::result::Result::Err(_) => ::serde_json::Value::Null,
                        },
                    )
                }
            });
            let after_pairs = tracked.iter().map(|c| {
                let column_lit = c.column.as_str();
                let ident = &c.ident;
                quote! {
                    (
                        #column_lit,
                        ::serde_json::to_value(&self.#ident)
                            .unwrap_or(::serde_json::Value::Null),
                    )
                }
            });
            let pk_str = audit_pk_to_string.clone();
            let pre = quote! {
                let _audit_select_sql = ::std::format!(
                    r#"SELECT {} FROM "{}" WHERE "{}" = $1"#,
                    #select_cols_lit,
                    <Self as ::rustango::core::Model>::SCHEMA.table,
                    #pk_column_lit_for_select,
                );
                let _audit_before_pairs:
                    ::std::option::Option<::std::vec::Vec<(&'static str, ::serde_json::Value)>> =
                    match ::rustango::sql::sqlx::query(&_audit_select_sql)
                        .bind(#pk_value_for_bind)
                        .fetch_optional(&mut *_executor)
                        .await
                    {
                        ::core::result::Result::Ok(::core::option::Option::Some(_audit_before_row)) => {
                            ::core::option::Option::Some(::std::vec![ #( #before_pairs ),* ])
                        }
                        _ => ::core::option::Option::None,
                    };
            };
            let post = quote! {
                if let ::core::option::Option::Some(_audit_before) = _audit_before_pairs {
                    let _audit_after:
                        ::std::vec::Vec<(&'static str, ::serde_json::Value)> =
                        ::std::vec![ #( #after_pairs ),* ];
                    let _audit_entry = ::rustango::audit::PendingEntry {
                        entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                        entity_pk: #pk_str,
                        operation: ::rustango::audit::AuditOp::Update,
                        source: ::rustango::audit::current_source(),
                        changes: ::rustango::audit::diff_changes(
                            &_audit_before,
                            &_audit_after,
                        ),
                    };
                    ::rustango::audit::emit_one(&mut *_executor, &_audit_entry).await?;
                }
            };
            (pre, post)
        }
    } else {
        (quote!(), quote!())
    };

    // Bulk-insert audit: capture every row's tracked fields after the
    // RETURNING populates each PK, then push one batched INSERT INTO
    // audit_log via `emit_many`. One round-trip regardless of N rows.
    let audit_bulk_insert_emit: TokenStream2 = if audited_fields.is_some() {
        let row_pk_str = if let Some((pk_ident, _)) = primary_key {
            if fields.pk_is_auto {
                quote!(_row.#pk_ident.get().map(|v| ::std::format!("{}", v)).unwrap_or_default())
            } else {
                quote!(::std::format!("{}", &_row.#pk_ident))
            }
        } else {
            quote!(::std::string::String::new())
        };
        let row_pairs = audited_fields.unwrap_or(&[]).iter().map(|c| {
            let column_lit = c.column.as_str();
            let ident = &c.ident;
            quote! {
                (
                    #column_lit,
                    ::serde_json::to_value(&_row.#ident)
                        .unwrap_or(::serde_json::Value::Null),
                )
            }
        });
        quote! {
            let _audit_source = ::rustango::audit::current_source();
            let mut _audit_entries:
                ::std::vec::Vec<::rustango::audit::PendingEntry> =
                    ::std::vec::Vec::with_capacity(rows.len());
            for _row in rows.iter() {
                _audit_entries.push(::rustango::audit::PendingEntry {
                    entity_table: <Self as ::rustango::core::Model>::SCHEMA.table,
                    entity_pk: #row_pk_str,
                    operation: ::rustango::audit::AuditOp::Create,
                    source: _audit_source.clone(),
                    changes: ::rustango::audit::snapshot_changes(&[
                        #( #row_pairs ),*
                    ]),
                });
            }
            ::rustango::audit::emit_many(&mut *_executor, &_audit_entries).await?;
        }
    } else {
        quote!()
    };

    let save_method = if fields.pk_is_auto {
        let (pk_ident, pk_column) = primary_key.expect("pk_is_auto implies primary_key is Some");
        let pk_column_lit = pk_column.as_str();
        let assignments = &fields.update_assignments;
        let upsert_cols = &fields.upsert_update_columns;
        let upsert_pushes = &fields.insert_pushes;
        let upsert_returning = &fields.returning_cols;
        let upsert_auto_assigns = &fields.auto_assigns;
        // Conflict target: prefer the first declared `unique_together`
        // when it exists. Plain `Auto<T>` PKs are server-assigned via
        // `BIGSERIAL` and never collide on insert, so a PK-only target
        // would silently turn `upsert()` into "always-insert" for
        // surrogate-PK models with composite UNIQUE constraints — see
        // `RolePermission` / `UserRole` / `UserPermission` in the
        // tenancy permission engine. When no `unique_together` is
        // declared we keep the PK target (the original behaviour).
        let upsert_target_columns: Vec<String> = indexes
            .iter()
            .find(|i| i.unique && !i.columns.is_empty())
            .map(|i| i.columns.clone())
            .unwrap_or_else(|| vec![pk_column.clone()]);
        let upsert_target_lits = upsert_target_columns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let conflict_clause = if fields.upsert_update_columns.is_empty() {
            quote!(::rustango::core::ConflictClause::DoNothing)
        } else {
            quote!(::rustango::core::ConflictClause::DoUpdate {
                target: ::std::vec![ #( #upsert_target_lits ),* ],
                update_columns: ::std::vec![ #( #upsert_cols ),* ],
            })
        };
        Some(quote! {
            /// Insert this row if its `Auto<T>` primary key is
            /// `Unset`, otherwise update the existing row matching the
            /// PK. Mirrors Django's `save()` — caller doesn't need to
            /// pick `insert` vs the bulk-update path manually.
            ///
            /// On the insert branch, populates the PK from `RETURNING`
            /// (same behavior as `insert`). On the update branch,
            /// writes every non-PK column back; if no row matches the
            /// PK, returns `Ok(())` silently.
            ///
            /// Only generated when the primary key is declared as
            /// `Auto<T>`. Models with a manually-managed PK must use
            /// `insert` or the QuerySet update builder.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for SQL-writing
            /// or driver failures.
            #[cfg(feature = "postgres")]
            pub async fn save(
                &mut self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                #pool_to_save_on
            }

            /// Like [`Self::save`] but accepts any sqlx executor —
            /// `&PgPool`, `&mut PgConnection`, or a transaction. The
            /// escape hatch for tenant-scoped writes: schema-mode
            /// tenants share the registry pool but rely on a per-
            /// checkout `SET search_path`, so passing `&PgPool` would
            /// silently hit the wrong schema. Acquire a connection
            /// via `TenantPools::acquire(&org)` and pass `&mut *conn`.
            ///
            /// # Errors
            /// As [`Self::save`].
            #[cfg(feature = "postgres")]
            pub async fn save_on #executor_generics (
                &mut self,
                #executor_param,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                if matches!(self.#pk_ident, ::rustango::sql::Auto::Unset) {
                    return self.insert_on(#executor_passes_to_data_write).await;
                }
                #audit_update_pre
                let _query = ::rustango::core::UpdateQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    set: ::std::vec![ #( #assignments ),* ],
                    where_clause: ::rustango::core::WhereExpr::Predicate(
                        ::rustango::core::Filter {
                            column: #pk_column_lit,
                            op: ::rustango::core::Op::Eq,
                            value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                ::core::clone::Clone::clone(&self.#pk_ident)
                            ),
                        }
                    ),
                };
                let _ = ::rustango::sql::__macro_internals::update_on(
                    #executor_passes_to_data_write,
                    &_query,
                ).await?;
                #audit_update_post
                ::core::result::Result::Ok(())
            }

            /// Per-call override for the audit source. Runs
            /// [`Self::save_on`] inside an [`::rustango::audit::with_source`]
            /// scope so the resulting audit entry records `source`
            /// instead of the task-local default. Useful for seed
            /// scripts and one-off CLI tools that don't sit inside an
            /// admin handler. The override applies only to this call;
            /// no global state changes.
            ///
            /// # Errors
            /// As [`Self::save_on`].
            #[cfg(feature = "postgres")]
            pub async fn save_on_with #executor_generics (
                &mut self,
                #executor_param,
                source: ::rustango::audit::AuditSource,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                ::rustango::audit::with_source(source, self.save_on(_executor)).await
            }

            /// Insert this row or update it in-place if the primary key already
            /// exists — single round-trip via `INSERT … ON CONFLICT (pk) DO UPDATE`.
            ///
            /// With `Auto::Unset` PK the server assigns a new key and no conflict
            /// can occur (equivalent to `insert`). With `Auto::Set` PK the row is
            /// inserted if absent or all non-PK columns are overwritten if present.
            ///
            /// # Errors
            /// As [`Self::insert_on`].
            #[cfg(feature = "postgres")]
            pub async fn upsert(
                &mut self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                #pool_to_upsert_on
            }

            /// Like [`Self::upsert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::upsert`].
            #[cfg(feature = "postgres")]
            pub async fn upsert_on #executor_generics (
                &mut self,
                #executor_param,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                let mut _columns: ::std::vec::Vec<&'static str> =
                    ::std::vec::Vec::new();
                let mut _values: ::std::vec::Vec<::rustango::core::SqlValue> =
                    ::std::vec::Vec::new();
                #( #upsert_pushes )*
                let query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: _columns,
                    values: _values,
                    returning: ::std::vec![ #( #upsert_returning ),* ],
                    on_conflict: ::core::option::Option::Some(#conflict_clause),
                };
                let _returning_row_v = ::rustango::sql::__macro_internals::insert_returning_on(
                    #executor_passes_to_data_write,
                    &query,
                ).await?;
                let _returning_row = &_returning_row_v;
                #( #upsert_auto_assigns )*
                ::core::result::Result::Ok(())
            }
        })
    } else {
        None
    };

    let pk_methods = primary_key.map(|(pk_ident, pk_column)| {
        let pk_column_lit = pk_column.as_str();
        // Optional `soft_delete_on` / `restore_on` companions when the
        // model has a `#[rustango(soft_delete)]` column. They land
        // alongside the regular `delete_on` so callers have both
        // options — a hard delete (audit-tracked as a real DELETE) and
        // a logical delete (audit-tracked as an UPDATE setting the
        // deleted_at column to NOW()).
        let soft_delete_methods = if let Some(col) = fields.soft_delete_column.as_deref() {
            let col_lit = col;
            quote! {
                /// Soft-delete this row by setting its
                /// `#[rustango(soft_delete)]` column to `NOW()`.
                /// Mirrors Django's `SoftDeleteModel.delete()` shape:
                /// the row stays in the table; query helpers can
                /// filter it out by checking the column for `IS NOT
                /// NULL`.
                ///
                /// # Errors
                /// As [`Self::delete`].
                pub async fn soft_delete_on #executor_generics (
                    &self,
                    #executor_param,
                ) -> ::core::result::Result<u64, ::rustango::sql::ExecError>
                #executor_where
                {
                    let _query = ::rustango::core::UpdateQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        set: ::std::vec![
                            ::rustango::core::Assignment {
                                column: #col_lit,
                                value: ::core::convert::Into::<::rustango::core::Expr>::into(
                                    ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                        ::chrono::Utc::now()
                                    )
                                ),
                            },
                        ],
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    let _affected = ::rustango::sql::__macro_internals::update_on(
                        #executor_passes_to_data_write,
                        &_query,
                    ).await?;
                    #audit_softdelete_emit
                    ::core::result::Result::Ok(_affected)
                }

                /// Inverse of [`Self::soft_delete_on`] — clears the
                /// soft-delete column back to NULL so the row is
                /// considered live again.
                ///
                /// # Errors
                /// As [`Self::delete`].
                pub async fn restore_on #executor_generics (
                    &self,
                    #executor_param,
                ) -> ::core::result::Result<u64, ::rustango::sql::ExecError>
                #executor_where
                {
                    let _query = ::rustango::core::UpdateQuery {
                        model: <Self as ::rustango::core::Model>::SCHEMA,
                        set: ::std::vec![
                            ::rustango::core::Assignment {
                                column: #col_lit,
                                value: ::core::convert::Into::<::rustango::core::Expr>::into(
                                    ::rustango::core::SqlValue::Null
                                ),
                            },
                        ],
                        where_clause: ::rustango::core::WhereExpr::Predicate(
                            ::rustango::core::Filter {
                                column: #pk_column_lit,
                                op: ::rustango::core::Op::Eq,
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::core::clone::Clone::clone(&self.#pk_ident)
                                ),
                            }
                        ),
                    };
                    let _affected = ::rustango::sql::__macro_internals::update_on(
                        #executor_passes_to_data_write,
                        &_query,
                    ).await?;
                    #audit_restore_emit
                    ::core::result::Result::Ok(_affected)
                }
            }
        } else {
            quote!()
        };
        quote! {
            /// Delete the row identified by this instance's primary key.
            ///
            /// Returns the number of rows affected (0 or 1).
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for SQL-writing or
            /// driver failures.
            #[cfg(feature = "postgres")]
            pub async fn delete(
                &self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<u64, ::rustango::sql::ExecError> {
                #pool_to_delete_on
            }

            /// Like [`Self::delete`] but accepts any sqlx executor —
            /// for tenant-scoped deletes against an explicitly-acquired
            /// connection. See [`Self::save_on`] for the rationale.
            ///
            /// # Errors
            /// As [`Self::delete`].
            #[cfg(feature = "postgres")]
            pub async fn delete_on #executor_generics (
                &self,
                #executor_param,
            ) -> ::core::result::Result<u64, ::rustango::sql::ExecError>
            #executor_where
            {
                let query = ::rustango::core::DeleteQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    where_clause: ::rustango::core::WhereExpr::Predicate(
                        ::rustango::core::Filter {
                            column: #pk_column_lit,
                            op: ::rustango::core::Op::Eq,
                            value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                ::core::clone::Clone::clone(&self.#pk_ident)
                            ),
                        }
                    ),
                };
                let _affected = ::rustango::sql::__macro_internals::delete_on(
                    #executor_passes_to_data_write,
                    &query,
                ).await?;
                #audit_delete_emit
                ::core::result::Result::Ok(_affected)
            }

            /// Per-call audit-source override for [`Self::delete_on`].
            /// See [`Self::save_on_with`] for shape rationale.
            ///
            /// # Errors
            /// As [`Self::delete_on`].
            #[cfg(feature = "postgres")]
            pub async fn delete_on_with #executor_generics (
                &self,
                #executor_param,
                source: ::rustango::audit::AuditSource,
            ) -> ::core::result::Result<u64, ::rustango::sql::ExecError>
            #executor_where
            {
                ::rustango::audit::with_source(source, self.delete_on(_executor)).await
            }
            #pool_delete_method
            #pool_insert_method
            #pool_save_method
            #tx_delete_method
            #tx_insert_method
            #tx_save_method
            #soft_delete_methods
        }
    });

    let insert_method = if fields.has_auto {
        let pushes = &fields.insert_pushes;
        let returning_cols = &fields.returning_cols;
        let auto_assigns = &fields.auto_assigns;
        quote! {
            /// Insert this row into its table. Skips columns whose
            /// `Auto<T>` value is `Unset` so Postgres' SERIAL/BIGSERIAL
            /// sequence fills them in, then reads each `Auto` column
            /// back via `RETURNING` and stores it on `self`.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for SQL-writing or
            /// driver failures.
            #[cfg(feature = "postgres")]
            pub async fn insert(
                &mut self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                #pool_to_insert_on
            }

            /// Like [`Self::insert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::insert`].
            #[cfg(feature = "postgres")]
            pub async fn insert_on #executor_generics (
                &mut self,
                #executor_param,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                let mut _columns: ::std::vec::Vec<&'static str> =
                    ::std::vec::Vec::new();
                let mut _values: ::std::vec::Vec<::rustango::core::SqlValue> =
                    ::std::vec::Vec::new();
                #( #pushes )*
                let query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: _columns,
                    values: _values,
                    returning: ::std::vec![ #( #returning_cols ),* ],
                    on_conflict: ::core::option::Option::None,
                };
                let _returning_row_v = ::rustango::sql::__macro_internals::insert_returning_on(
                    #executor_passes_to_data_write,
                    &query,
                ).await?;
                let _returning_row = &_returning_row_v;
                #( #auto_assigns )*
                #audit_insert_emit
                ::core::result::Result::Ok(())
            }

            /// Per-call audit-source override for [`Self::insert_on`].
            /// See [`Self::save_on_with`] for shape rationale.
            ///
            /// # Errors
            /// As [`Self::insert_on`].
            #[cfg(feature = "postgres")]
            pub async fn insert_on_with #executor_generics (
                &mut self,
                #executor_param,
                source: ::rustango::audit::AuditSource,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                ::rustango::audit::with_source(source, self.insert_on(_executor)).await
            }
        }
    } else {
        let insert_columns = &fields.insert_columns;
        let insert_values = &fields.insert_values;
        quote! {
            /// Insert this row into its table.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for SQL-writing or
            /// driver failures.
            #[cfg(feature = "postgres")]
            pub async fn insert(
                &self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                self.insert_on(pool).await
            }

            /// Like [`Self::insert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::insert`].
            #[cfg(feature = "postgres")]
            pub async fn insert_on<'_c, _E>(
                &self,
                _executor: _E,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            where
                _E: ::rustango::sql::sqlx::Executor<'_c, Database = ::rustango::sql::sqlx::Postgres>,
            {
                let query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #insert_columns ),* ],
                    values: ::std::vec![ #( #insert_values ),* ],
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::None,
                };
                ::rustango::sql::__macro_internals::insert_on(_executor, &query).await
            }
        }
    };

    let bulk_insert_method = if fields.has_auto {
        let cols_no_auto = &fields.bulk_columns_no_auto;
        let cols_all = &fields.bulk_columns_all;
        let pushes_no_auto = &fields.bulk_pushes_no_auto;
        let pushes_all = &fields.bulk_pushes_all;
        let returning_cols = &fields.returning_cols;
        let auto_assigns_for_row = bulk_auto_assigns_for_row(fields);
        let uniformity = &fields.bulk_auto_uniformity;
        let first_auto_ident = fields
            .first_auto_ident
            .as_ref()
            .expect("has_auto implies first_auto_ident is Some");
        quote! {
            /// Bulk-insert `rows` in a single round-trip. Every row's
            /// `Auto<T>` PK fields must uniformly be `Auto::Unset`
            /// (sequence fills them in) or uniformly `Auto::Set(_)`
            /// (caller-supplied values). Mixed Set/Unset is rejected
            /// — call `insert` per row for that case.
            ///
            /// Empty slice is a no-op. Each row's `Auto` fields are
            /// populated from the `RETURNING` clause in input order
            /// before this returns.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for validation,
            /// SQL-writing, mixed-Auto rejection, or driver failures.
            #[cfg(feature = "postgres")]
            pub async fn bulk_insert(
                rows: &mut [Self],
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                #pool_to_bulk_insert_on
            }

            /// Like [`Self::bulk_insert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::bulk_insert`].
            #[cfg(feature = "postgres")]
            pub async fn bulk_insert_on #executor_generics (
                rows: &mut [Self],
                #executor_param,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            #executor_where
            {
                if rows.is_empty() {
                    return ::core::result::Result::Ok(());
                }
                let _first_unset = matches!(
                    rows[0].#first_auto_ident,
                    ::rustango::sql::Auto::Unset
                );
                #( #uniformity )*

                let mut _all_rows: ::std::vec::Vec<
                    ::std::vec::Vec<::rustango::core::SqlValue>,
                > = ::std::vec::Vec::with_capacity(rows.len());
                let _columns: ::std::vec::Vec<&'static str> = if _first_unset {
                    for _row in rows.iter() {
                        let mut _row_vals: ::std::vec::Vec<::rustango::core::SqlValue> =
                            ::std::vec::Vec::new();
                        #( #pushes_no_auto )*
                        _all_rows.push(_row_vals);
                    }
                    ::std::vec![ #( #cols_no_auto ),* ]
                } else {
                    for _row in rows.iter() {
                        let mut _row_vals: ::std::vec::Vec<::rustango::core::SqlValue> =
                            ::std::vec::Vec::new();
                        #( #pushes_all )*
                        _all_rows.push(_row_vals);
                    }
                    ::std::vec![ #( #cols_all ),* ]
                };

                let _query = ::rustango::core::BulkInsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: _columns,
                    rows: _all_rows,
                    returning: ::std::vec![ #( #returning_cols ),* ],
                    on_conflict: ::core::option::Option::None,
                };
                let _returned = ::rustango::sql::__macro_internals::bulk_insert_on(
                    #executor_passes_to_data_write,
                    &_query,
                ).await?;
                if _returned.len() != rows.len() {
                    return ::core::result::Result::Err(
                        ::rustango::sql::ExecError::Sql(
                            ::rustango::sql::SqlError::BulkInsertReturningMismatch {
                                expected: rows.len(),
                                actual: _returned.len(),
                            }
                        )
                    );
                }
                for (_returning_row, _row_mut) in _returned.iter().zip(rows.iter_mut()) {
                    #auto_assigns_for_row
                }
                #audit_bulk_insert_emit
                ::core::result::Result::Ok(())
            }
        }
    } else {
        let cols_all = &fields.bulk_columns_all;
        let pushes_all = &fields.bulk_pushes_all;
        quote! {
            /// Bulk-insert `rows` in a single round-trip. Every row's
            /// fields are written verbatim — there are no `Auto<T>`
            /// fields on this model.
            ///
            /// Empty slice is a no-op.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for validation,
            /// SQL-writing, or driver failures.
            #[cfg(feature = "postgres")]
            pub async fn bulk_insert(
                rows: &[Self],
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                Self::bulk_insert_on(rows, pool).await
            }

            /// Like [`Self::bulk_insert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::bulk_insert`].
            #[cfg(feature = "postgres")]
            pub async fn bulk_insert_on<'_c, _E>(
                rows: &[Self],
                _executor: _E,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError>
            where
                _E: ::rustango::sql::sqlx::Executor<'_c, Database = ::rustango::sql::sqlx::Postgres>,
            {
                if rows.is_empty() {
                    return ::core::result::Result::Ok(());
                }
                let mut _all_rows: ::std::vec::Vec<
                    ::std::vec::Vec<::rustango::core::SqlValue>,
                > = ::std::vec::Vec::with_capacity(rows.len());
                for _row in rows.iter() {
                    let mut _row_vals: ::std::vec::Vec<::rustango::core::SqlValue> =
                        ::std::vec::Vec::new();
                    #( #pushes_all )*
                    _all_rows.push(_row_vals);
                }
                let _query = ::rustango::core::BulkInsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #cols_all ),* ],
                    rows: _all_rows,
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::None,
                };
                let _ = ::rustango::sql::__macro_internals::bulk_insert_on(_executor, &_query).await?;
                ::core::result::Result::Ok(())
            }
        }
    };

    // Tri-dialect `bulk_upsert_pool` — issue #267 / T1.5. Always emitted
    // (no postgres-feature gate); routes through the existing
    // `bulk_insert_pool` + per-dialect conflict writer.
    //
    // Auto<T> PKs are required to be `Auto::Unset` for every row so the
    // sequence picks the PK for fresh inserts; the UPDATE branch never
    // touches the Auto column.
    let bulk_upsert_pool_method = {
        // Pick the "no Auto" columns when the model has Auto fields,
        // else every column.
        let (upsert_cols, upsert_pushes): (Vec<_>, Vec<_>) = if fields.has_auto {
            (
                fields.bulk_columns_no_auto.clone(),
                fields.bulk_pushes_no_auto.clone(),
            )
        } else {
            (
                fields.bulk_columns_all.clone(),
                fields.bulk_pushes_all.clone(),
            )
        };
        quote! {
            /// Tri-dialect `bulk_create(update_conflicts=True)` — Django's
            /// canonical "import a batch idempotently" shape. Issue #267
            /// / T1.5.
            ///
            /// Per-row values are extracted and lowered into a
            /// [`::rustango::core::BulkInsertQuery`] with
            /// `on_conflict = DoUpdate { target, update_columns }`. The
            /// writer dispatches per-dialect:
            /// * Postgres / SQLite: `INSERT … ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col`
            /// * MySQL: `INSERT … ON DUPLICATE KEY UPDATE col = VALUES(col)` (target ignored — MySQL matches every UNIQUE index)
            ///
            /// `target` names the column(s) whose unique constraint
            /// defines the conflict (typically a `unique` or
            /// `unique_together` natural-key column, NOT the `Auto<T>`
            /// PK). `update_cols` names the columns to overwrite on
            /// conflict — every other column is left untouched on the
            /// existing row.
            ///
            /// Auto-PK rows must all have `Auto::Unset` (the sequence
            /// picks the PK on insert; the update path never touches
            /// the Auto column). Auto-set rows trigger a hard error.
            /// Empty slice is a no-op.
            ///
            /// # Errors
            /// Returns [`::rustango::sql::ExecError`] for validation,
            /// SQL-writing, or driver failures.
            pub async fn bulk_upsert_pool(
                rows: &[Self],
                target: &[&'static str],
                update_cols: &[&'static str],
                pool: &::rustango::sql::Pool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                if rows.is_empty() {
                    return ::core::result::Result::Ok(());
                }
                let mut _all_rows: ::std::vec::Vec<
                    ::std::vec::Vec<::rustango::core::SqlValue>,
                > = ::std::vec::Vec::with_capacity(rows.len());
                for _row in rows.iter() {
                    let mut _row_vals: ::std::vec::Vec<::rustango::core::SqlValue> =
                        ::std::vec::Vec::new();
                    #( #upsert_pushes )*
                    _all_rows.push(_row_vals);
                }
                let _query = ::rustango::core::BulkInsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #upsert_cols ),* ],
                    rows: _all_rows,
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::Some(
                        ::rustango::core::ConflictClause::DoUpdate {
                            target: target.to_vec(),
                            update_columns: update_cols.to_vec(),
                        }
                    ),
                };
                ::rustango::sql::bulk_insert_pool(pool, &_query).await
            }

            /// Tri-dialect `bulk_create(ignore_conflicts=True)` — silently
            /// skip rows that would violate a unique constraint. Issue
            /// #267 / T1.5. Same per-dialect dispatch as
            /// [`Self::bulk_upsert_pool`] but with `ON CONFLICT … DO
            /// NOTHING` (Postgres / SQLite) / `ON DUPLICATE KEY UPDATE
            /// <pivot> = <pivot>` (MySQL no-op write).
            ///
            /// # Errors
            /// As [`Self::bulk_upsert_pool`].
            pub async fn bulk_insert_or_ignore_pool(
                rows: &[Self],
                pool: &::rustango::sql::Pool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                if rows.is_empty() {
                    return ::core::result::Result::Ok(());
                }
                let mut _all_rows: ::std::vec::Vec<
                    ::std::vec::Vec<::rustango::core::SqlValue>,
                > = ::std::vec::Vec::with_capacity(rows.len());
                for _row in rows.iter() {
                    let mut _row_vals: ::std::vec::Vec<::rustango::core::SqlValue> =
                        ::std::vec::Vec::new();
                    #( #upsert_pushes )*
                    _all_rows.push(_row_vals);
                }
                let _query = ::rustango::core::BulkInsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #upsert_cols ),* ],
                    rows: _all_rows,
                    returning: ::std::vec::Vec::new(),
                    on_conflict: ::core::option::Option::Some(
                        ::rustango::core::ConflictClause::DoNothing
                    ),
                };
                ::rustango::sql::bulk_insert_pool(pool, &_query).await
            }
        }
    };

    let pk_value_helper = primary_key.map(|(pk_ident, _)| {
        quote! {
            /// Hidden runtime accessor for the primary-key value as a
            /// [`SqlValue`]. Used by reverse-relation helpers
            /// (`<parent>::<child>_set`) emitted from sibling models'
            /// FK fields. Not part of the public API.
            #[doc(hidden)]
            pub fn __rustango_pk_value(&self) -> ::rustango::core::SqlValue {
                ::core::convert::Into::<::rustango::core::SqlValue>::into(
                    ::core::clone::Clone::clone(&self.#pk_ident)
                )
            }
        }
    });

    let has_pk_value_impl = primary_key.map(|(pk_ident, _)| {
        quote! {
            impl ::rustango::sql::HasPkValue for #struct_name {
                fn __rustango_pk_value_impl(&self) -> ::rustango::core::SqlValue {
                    ::core::convert::Into::<::rustango::core::SqlValue>::into(
                        ::core::clone::Clone::clone(&self.#pk_ident)
                    )
                }
            }
        }
    });

    let fk_pk_access_impl = fk_pk_access_impl_tokens(struct_name, &fields.fk_relations);

    // Slice 17.1 — `AssignAutoPkPool` impl lets `apply_auto_pk`
    // dispatch to the right per-backend body without the macro emitting
    // any `#[cfg(feature = …)]` arm into consumer code. Always emitted
    // so audited models with non-Auto PKs (which still go through
    // `insert_one_with_audit` → `apply_auto_pk`) link.
    let assign_auto_pk_pool_impl = {
        let auto_assigns = &fields.auto_assigns;
        // SQLite ≥ 3.35 supports the same RETURNING shape as Postgres,
        // so the body is structurally identical to `auto_assigns` —
        // only the helper name swaps from `try_get_returning` to
        // `try_get_returning_sqlite` so the closure typechecks against
        // a `SqliteRow` instead of a `PgRow`.
        let auto_assigns_sqlite: Vec<TokenStream2> = fields
            .auto_field_idents
            .iter()
            .map(|(ident, column)| {
                quote! {
                    self.#ident = ::rustango::sql::try_get_returning_sqlite(
                        _returning_row, #column
                    )?;
                }
            })
            .collect();
        let mysql_body = if let Some(first) = fields.first_auto_ident.as_ref() {
            // The MySQL `LAST_INSERT_ID()` is always i64. Route through
            // `MysqlAutoIdSet` so Auto<i32> narrows safely and
            // Auto<Uuid>/etc. fail to link against MySQL (intended —
            // those models can't use AUTO_INCREMENT). The trait is only
            // touched on the MySQL arm at runtime, so PG-only consumers
            // never see the bound failure.
            //
            // Pre-v0.20: models with multiple `Auto<T>` fields (e.g.
            // Auto<i64> PK + auto_now_add timestamp) errored hard at
            // runtime with "multi-column RETURNING". MySQL has no
            // multi-column RETURNING semantic and a follow-up SELECT
            // would need cross-trait plumbing. Pragmatic shape: succeed
            // with the FIRST Auto field populated from LAST_INSERT_ID();
            // any other Auto fields stay `Auto::Unset`. Callers that
            // need the DB-defaulted timestamp / UUID can re-fetch the
            // row by PK after `save_pool`. Fixes the cookbook chapter
            // 12 dialect divergence.
            let value_ty = fields
                .first_auto_value_ty
                .as_ref()
                .expect("first_auto_value_ty set whenever first_auto_ident is");
            quote! {
                let _converted = <#value_ty as ::rustango::sql::MysqlAutoIdSet>
                    ::rustango_from_mysql_auto_id(_id)?;
                self.#first = ::rustango::sql::Auto::Set(_converted);
                ::core::result::Result::Ok(())
            }
        } else {
            quote! {
                let _ = _id;
                ::core::result::Result::Ok(())
            }
        };
        quote! {
            impl ::rustango::sql::AssignAutoPkPool for #struct_name {
                fn __rustango_assign_from_pg_row(
                    &mut self,
                    _returning_row: &::rustango::sql::PgReturningRow,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    #( #auto_assigns )*
                    ::core::result::Result::Ok(())
                }
                fn __rustango_assign_from_mysql_id(
                    &mut self,
                    _id: i64,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    #mysql_body
                }
                fn __rustango_assign_from_sqlite_row(
                    &mut self,
                    _returning_row: &::rustango::sql::SqliteReturningRow,
                ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                    #( #auto_assigns_sqlite )*
                    ::core::result::Result::Ok(())
                }
            }
        }
    };

    let from_aliased_row_inits = &fields.from_aliased_row_inits;
    let aliased_row_helper = quote! {
        /// Decode a row's aliased target columns (produced by
        /// `select_related`'s LEFT JOIN) into a fresh instance of
        /// this model. Reads each column via
        /// `format!("{prefix}__{col}")`, matching the alias the
        /// SELECT writer emitted. Slice 9.0d.
        #[doc(hidden)]
        #[cfg(feature = "postgres")]
        pub fn __rustango_from_aliased_row(
            row: &::rustango::sql::sqlx::postgres::PgRow,
            prefix: &str,
        ) -> ::core::result::Result<Self, ::rustango::sql::sqlx::Error> {
            ::core::result::Result::Ok(Self {
                #( #from_aliased_row_inits ),*
            })
        }
    };
    // v0.23.0-batch8 — MySQL counterpart, gated through the
    // cfg-aware macro_rules so PG-only builds expand to nothing.
    let aliased_row_helper_my = quote! {
        ::rustango::__impl_my_aliased_row_decoder!(#struct_name, |row, prefix| {
            #( #from_aliased_row_inits ),*
        });
    };

    // v0.27 Phase 3 — SQLite counterpart, same hygiene-aware closure
    // pattern + cfg gate on the `sqlite` feature.
    let aliased_row_helper_sqlite = quote! {
        ::rustango::__impl_sqlite_aliased_row_decoder!(#struct_name, |row, prefix| {
            #( #from_aliased_row_inits ),*
        });
    };

    let load_related_impl = load_related_impl_tokens(struct_name, &fields.fk_relations);
    let load_related_impl_my = load_related_impl_my_tokens(struct_name, &fields.fk_relations);
    let load_related_impl_sqlite =
        load_related_impl_sqlite_tokens(struct_name, &fields.fk_relations);

    // Issue #289 / T2.6 — `#[rustango(manager_fn = "active")]` emits
    // extra `Self::<name>() -> QuerySet<Self>` accessors next to the
    // default `Self::objects()`. Each accessor returns a fresh
    // QuerySet that resolves any `impl <FooManagerExt> for QuerySet<Foo>`
    // methods the user defined.
    let extra_manager_fns: Vec<TokenStream2> = manager_fns
        .iter()
        .map(|fn_ident| {
            let model_name_str = struct_name.to_string();
            let fn_name_str = fn_ident.to_string();
            let doc = format!(
                "Custom-named QuerySet accessor for [`{model_name_str}`]. \
                 Generated by `#[rustango(manager_fn = \"{fn_name_str}\")]` — \
                 equivalent to `Self::objects()`. Chains with any \
                 `impl ... for QuerySet<{model_name_str}> {{ ... }}` \
                 extension methods."
            );
            quote! {
                #[doc = #doc]
                #[must_use]
                pub fn #fn_ident() -> ::rustango::query::QuerySet<#struct_name> {
                    ::rustango::query::QuerySet::new()
                }
            }
        })
        .collect();

    quote! {
        impl #struct_name {
            /// Start a new `QuerySet` over this model.
            #[must_use]
            pub fn objects() -> ::rustango::query::QuerySet<#struct_name> {
                ::rustango::query::QuerySet::new()
            }

            #( #extra_manager_fns )*

            #insert_method

            #bulk_insert_method

            #bulk_upsert_pool_method

            #save_method

            #pk_methods

            #pk_value_helper

            #aliased_row_helper

            #column_consts
        }

        #aliased_row_helper_my

        #aliased_row_helper_sqlite

        #load_related_impl

        #load_related_impl_my

        #load_related_impl_sqlite

        #has_pk_value_impl

        #fk_pk_access_impl

        #assign_auto_pk_pool_impl
    }
}

/// Per-row Auto-field assigns for `bulk_insert` — equivalent to
/// `auto_assigns` but reading from `_returning_row` and writing to
/// `_row_mut` instead of `self`.
fn bulk_auto_assigns_for_row(fields: &CollectedFields) -> TokenStream2 {
    let lines = fields.auto_field_idents.iter().map(|(ident, column)| {
        let col_lit = column.as_str();
        quote! {
            _row_mut.#ident = ::rustango::sql::sqlx::Row::try_get(
                _returning_row,
                #col_lit,
            )?;
        }
    });
    quote! { #( #lines )* }
}

/// Emit `pub const id: …Id = …Id;` per field, inside the inherent impl.
fn column_const_tokens(module_ident: &syn::Ident, entries: &[ColumnEntry]) -> TokenStream2 {
    let lines = entries.iter().map(|e| {
        let ident = &e.ident;
        let col_ty = column_type_ident(ident);
        quote! {
            #[allow(non_upper_case_globals)]
            pub const #ident: #module_ident::#col_ty = #module_ident::#col_ty;
        }
    });
    quote! { #(#lines)* }
}

/// Emit a hidden per-model module carrying one zero-sized type per field,
/// each with a `Column` impl pointing back at the model.
fn column_module_tokens(
    module_ident: &syn::Ident,
    struct_name: &syn::Ident,
    entries: &[ColumnEntry],
) -> TokenStream2 {
    let items = entries.iter().map(|e| {
        let col_ty = column_type_ident(&e.ident);
        let value_ty = &e.value_ty;
        let name = &e.name;
        let column = &e.column;
        let field_type_tokens = &e.field_type_tokens;
        quote! {
            #[derive(::core::clone::Clone, ::core::marker::Copy)]
            pub struct #col_ty;

            impl ::rustango::core::Column for #col_ty {
                type Model = super::#struct_name;
                type Value = #value_ty;
                const NAME: &'static str = #name;
                const COLUMN: &'static str = #column;
                const FIELD_TYPE: ::rustango::core::FieldType = #field_type_tokens;
            }
        }
    });
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types, non_snake_case)]
        pub mod #module_ident {
            // Re-import the parent scope so field types referencing
            // sibling models (e.g. `ForeignKey<Author>`) resolve
            // inside this submodule. Without this we'd hit
            // `proc_macro_derive_resolution_fallback` warnings.
            #[allow(unused_imports)]
            use super::*;
            #(#items)*
        }
    }
}

fn column_type_ident(field_ident: &syn::Ident) -> syn::Ident {
    syn::Ident::new(&format!("{field_ident}_col"), field_ident.span())
}

fn column_module_ident(struct_name: &syn::Ident) -> syn::Ident {
    syn::Ident::new(
        &format!("__rustango_cols_{struct_name}"),
        struct_name.span(),
    )
}

fn from_row_impl_tokens(struct_name: &syn::Ident, from_row_inits: &[TokenStream2]) -> TokenStream2 {
    // The Postgres impl is always emitted — every rustango build pulls in
    // sqlx-postgres via the default `postgres` feature. The MySQL impl is
    // routed through `::rustango::__impl_my_from_row!`, a cfg-gated
    // macro_rules whose body collapses to nothing when rustango is built
    // without the `mysql` feature. No user-facing feature shim required.
    //
    // The macro_rules pattern expects `[ field: expr, … ]` — we need to
    // re-shape `from_row_inits` (each token is `field: row.try_get(...)`)
    // back into a comma-separated list inside square brackets. Since each
    // entry is already in `field: expr` shape, the existing tokens slot in.
    quote! {
        #[cfg(feature = "postgres")]
        impl<'r> ::rustango::sql::sqlx::FromRow<'r, ::rustango::sql::sqlx::postgres::PgRow>
            for #struct_name
        {
            fn from_row(
                row: &'r ::rustango::sql::sqlx::postgres::PgRow,
            ) -> ::core::result::Result<Self, ::rustango::sql::sqlx::Error> {
                ::core::result::Result::Ok(Self {
                    #( #from_row_inits ),*
                })
            }
        }

        ::rustango::__impl_my_from_row!(#struct_name, |row| {
            #( #from_row_inits ),*
        });

        ::rustango::__impl_sqlite_from_row!(#struct_name, |row| {
            #( #from_row_inits ),*
        });
    }
}

struct ContainerAttrs {
    table: Option<String>,
    display: Option<(String, proc_macro2::Span)>,
    /// Explicit Django-style app label from `#[rustango(app = "blog")]`.
    /// Recorded on the emitted `ModelSchema.app_label`. When unset,
    /// `ModelEntry::resolved_app_label()` infers from `module_path!()`
    /// at runtime — this attribute is the override for cases where
    /// the inference is wrong (e.g. a model that conceptually belongs
    /// to one app but is physically in another module).
    app: Option<String>,
    /// Django ModelAdmin-shape per-model knobs from
    /// `#[rustango(admin(...))]`. `None` when the user didn't write the
    /// attribute — the emitted `ModelSchema.admin` becomes `None` and
    /// admin code falls back to `AdminConfig::DEFAULT`.
    admin: Option<AdminAttrs>,
    /// Per-model audit configuration from `#[rustango(audit(...))]`.
    /// `None` when the model isn't audited — write paths emit no
    /// audit entries. When present, single-row writes capture
    /// before/after for the listed fields and bulk writes batch
    /// snapshots into one INSERT into `rustango_audit_log`.
    audit: Option<AuditAttrs>,
    /// `true` when `#[rustango(permissions)]` is present. Signals that
    /// `auto_create_permissions` should seed the four CRUD codenames for
    /// this model.
    permissions: bool,
    /// Many-to-many relations declared via
    /// `#[rustango(m2m(name = "tags", to = "app_tags", through = "post_tags",
    ///                 src = "post_id", dst = "tag_id"))]`.
    m2m: Vec<M2MAttr>,
    /// Composite indexes declared via
    /// `#[rustango(index("col1, col2"))]` or
    /// `#[rustango(index("col1, col2", unique, name = "my_idx"))]`.
    /// Single-column indexes from `#[rustango(index)]` on fields are
    /// accumulated here during field collection.
    indexes: Vec<IndexAttr>,
    /// Table-level CHECK constraints declared via
    /// `#[rustango(check(name = "…", expr = "…"))]`.
    checks: Vec<CheckAttr>,
    /// Composite (multi-column) FKs declared via
    /// `#[rustango(fk_composite(name = "…", to = "…", on = (…), from = (…)))]`.
    /// Sub-slice F.2 of the v0.15.0 ContentType plan.
    composite_fks: Vec<CompositeFkAttr>,
    /// Generic ("any model") FKs declared via
    /// `#[rustango(generic_fk(name = "…", ct_column = "…", pk_column = "…"))]`.
    /// Sub-slice F.4 of the v0.15.0 ContentType plan.
    generic_fks: Vec<GenericFkAttr>,
    /// Where this model lives in a tenancy deployment, declared via
    /// `#[rustango(scope = "registry")]` or `#[rustango(scope = "tenant")]`.
    /// Defaults to `"tenant"` when unset; `makemigrations` uses this
    /// to partition diff output between registry-scoped and
    /// tenant-scoped migration files.
    scope: Option<String>,
    /// Custom-Manager extension-trait name from
    /// `#[rustango(manager(ext = "FooManagerExt"))]`. Issue #271 / T1.9.
    /// When set, the macro emits an empty `pub trait <name>: Sized {}`
    /// adjacent to the model so users can write
    /// `impl FooManagerExt for QuerySet<Foo> { fn published(self) -> Self ... }`
    /// and discover the convention from the model definition.
    manager_ext: Option<syn::Ident>,
    /// Extra QuerySet accessor names from
    /// `#[rustango(manager_fn = "active")]`. Issue #289 / T2.6.
    /// Each value adds a `pub fn <name>() -> QuerySet<Self>` next to
    /// the default `Self::objects()`. Multiple attributes allowed.
    manager_fns: Vec<syn::Ident>,
    /// Default ordering declared via `#[rustango(default_order =
    /// "-created_at, status")]`. Issue #291 / T2.5. Each entry is
    /// `(column_name, desc_flag, span_for_error_reporting)` — the
    /// `-` prefix means descending; the `+` prefix or no prefix means
    /// ascending.
    default_order: Vec<(String, bool, proc_macro2::Span)>,
    /// `true` when `#[rustango(view)]` is present. Issue #293 / T2.10.
    /// Routes the emitted schema's `is_view = true` so the migration
    /// snapshot skips this model (its underlying SQL view is operator-
    /// managed, not rustango-managed).
    is_view: bool,
    /// `#[rustango(verbose_name = "blog post")]` — Django-shape
    /// human-readable singular label for the model. Threaded into
    /// `ModelSchema::verbose_name` so admin section headers /
    /// breadcrumbs / "Add X" buttons can prefer the friendly caption
    /// over the Rust struct identifier.
    verbose_name: Option<String>,
    /// `#[rustango(verbose_name_plural = "blog posts")]` — explicit
    /// plural form. Threaded into `ModelSchema::verbose_name_plural`.
    /// When unset, `display_label_plural()` auto-suffixes `s`.
    verbose_name_plural: Option<String>,
}

/// Parsed form of one index declaration (field-level or container-level).
struct IndexAttr {
    /// Index name; auto-derived when `None` at parse time.
    name: Option<String>,
    /// Column names in the index.
    columns: Vec<String>,
    /// `true` for `CREATE UNIQUE INDEX`.
    unique: bool,
    /// Access method token (`"btree"`, `"gin"`, `"gist"`, `"brin"`,
    /// `"spgist"`, `"hash"`, `"bloom"`). Issue #34. Defaults to
    /// `"btree"` when the attribute is absent — the DDL writer omits
    /// the `USING` clause and the backend uses its own default
    /// (btree on every supported dialect).
    method: String,
    /// Optional `WHERE <expr>` clause for partial indexes. Issue #265 /
    /// T1.3. Set via `#[rustango(unique_when(columns = "...",
    /// condition = "...", name = "..."))]`. `None` for plain indexes.
    where_clause: Option<String>,
}

/// Parsed form of one `#[rustango(check(name = "…", expr = "…"))]` declaration.
struct CheckAttr {
    name: String,
    expr: String,
}

/// Parsed form of one `#[rustango(fk_composite(name = "audit_target",
/// to = "rustango_audit_log", on = ("entity_table", "entity_pk"),
/// from = ("table_name", "row_pk")))]` declaration. Sub-slice F.2 of
/// the v0.15.0 ContentType plan — multi-column foreign keys live on
/// the model, not the field.
struct CompositeFkAttr {
    /// Logical relation name (free-form Rust identifier).
    name: String,
    /// SQL table name of the target.
    to: String,
    /// Source-side column names, in declaration order.
    from: Vec<String>,
    /// Target-side column names, same length / order as `from`.
    on: Vec<String>,
}

/// Parsed form of one `#[rustango(generic_fk(name = "target",
/// ct_column = "content_type_id", pk_column = "object_pk"))]`
/// declaration. Sub-slice F.4 of the v0.15.0 ContentType plan —
/// generic ("any model") FKs live on the model, not the field.
struct GenericFkAttr {
    /// Logical relation name (free-form Rust identifier).
    name: String,
    /// Source-side column carrying the `content_type_id` value.
    ct_column: String,
    /// Source-side column carrying the target row's primary key.
    pk_column: String,
}

/// Parsed form of one `#[rustango(m2m(...))]` declaration.
struct M2MAttr {
    /// Accessor suffix: `tags` → generates `tags_m2m()`.
    name: String,
    /// Target table (e.g. `"app_tags"`).
    to: String,
    /// Junction table (e.g. `"post_tags"`).
    through: String,
    /// Source FK column in the junction table (e.g. `"post_id"`).
    src: String,
    /// Destination FK column in the junction table (e.g. `"tag_id"`).
    dst: String,
}

/// Parsed shape of `#[rustango(audit(track = "name, body", source =
/// "user"))]`. `track` is a comma-separated list of field names whose
/// before/after values land in the JSONB `changes` column. `source`
/// is informational only — it pins a default source when the model
/// is written outside any `audit::with_source(...)` scope (rare).
#[derive(Default)]
struct AuditAttrs {
    /// Field names to capture in the `changes` JSONB. Validated
    /// against declared scalar fields at compile time. Empty means
    /// "track every scalar field" — Django's audit-everything default.
    track: Option<(Vec<String>, proc_macro2::Span)>,
}

/// Parsed shape of `#[rustango(admin(list_display = "…", search_fields =
/// "…", list_per_page = N, ordering = "…"))]`. Field-name lists are
/// comma-separated strings; we validate each ident against the model's
/// declared fields at compile time.
#[derive(Default)]
struct AdminAttrs {
    list_display: Option<(Vec<String>, proc_macro2::Span)>,
    search_fields: Option<(Vec<String>, proc_macro2::Span)>,
    list_per_page: Option<usize>,
    ordering: Option<(Vec<(String, bool)>, proc_macro2::Span)>,
    readonly_fields: Option<(Vec<String>, proc_macro2::Span)>,
    list_filter: Option<(Vec<String>, proc_macro2::Span)>,
    /// Bulk action names. No field-validation against model fields —
    /// these are action handlers, not column references.
    actions: Option<(Vec<String>, proc_macro2::Span)>,
    /// Form fieldsets — `Vec<(title, [field_names])>`. Pipe-separated
    /// sections, comma-separated fields per section, optional
    /// `Title:` prefix. Empty title omits the `<legend>`.
    fieldsets: Option<(Vec<(String, Vec<String>)>, proc_macro2::Span)>,
}

fn parse_container_attrs(input: &DeriveInput) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs {
        table: None,
        display: None,
        app: None,
        admin: None,
        audit: None,
        // Default `permissions = true` so every `#[derive(Model)]`
        // gets the four CRUD codenames seeded by `auto_create_permissions`
        // and is visible to non-superusers in the tenant admin without
        // manual per-model annotation. Models that intentionally don't
        // want permission rows (registry-internal types, framework
        // tables operators shouldn't manage directly) opt out via
        // `#[rustango(permissions = false)]`. v0.27.2 — fixes the
        // out-of-the-box admin invisibility regression (#62).
        permissions: true,
        m2m: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
        composite_fks: Vec::new(),
        generic_fks: Vec::new(),
        scope: None,
        manager_ext: None,
        manager_fns: Vec::new(),
        default_order: Vec::new(),
        is_view: false,
        verbose_name: None,
        verbose_name_plural: None,
    };
    for attr in &input.attrs {
        if !attr.path().is_ident("rustango") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let s: LitStr = meta.value()?.parse()?;
                let name = s.value();
                // v0.27.3 (#65) — macro-time guard against table names
                // that compile but break SQL downstream. Hyphens are
                // the common footgun: PostgreSQL accepts a quoted
                // `"intermediate-region"` in CREATE TABLE, but the
                // FK / index name derivation in `migrate::ddl`
                // emits `intermediate-region_field_fkey` unquoted,
                // which then fails the SQL parser. Same shape rule
                // as Postgres regular identifiers so the safe path
                // is the only path.
                validate_table_name(&name, s.span())?;
                out.table = Some(name);
                return Ok(());
            }
            if meta.path.is_ident("display") {
                let s: LitStr = meta.value()?.parse()?;
                out.display = Some((s.value(), s.span()));
                return Ok(());
            }
            if meta.path.is_ident("app") {
                let s: LitStr = meta.value()?.parse()?;
                out.app = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("scope") {
                let s: LitStr = meta.value()?.parse()?;
                let val = s.value();
                if !matches!(val.to_ascii_lowercase().as_str(), "registry" | "tenant") {
                    return Err(meta.error(format!(
                        "`scope` must be \"registry\" or \"tenant\", got {val:?}"
                    )));
                }
                out.scope = Some(val);
                return Ok(());
            }
            if meta.path.is_ident("admin") {
                let mut admin = AdminAttrs::default();
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("list_display") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.list_display =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    if inner.path.is_ident("search_fields") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.search_fields =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    if inner.path.is_ident("readonly_fields") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.readonly_fields =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    if inner.path.is_ident("list_per_page") {
                        let lit: syn::LitInt = inner.value()?.parse()?;
                        admin.list_per_page = Some(lit.base10_parse::<usize>()?);
                        return Ok(());
                    }
                    if inner.path.is_ident("ordering") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.ordering = Some((
                            parse_ordering_list(&s.value()),
                            s.span(),
                        ));
                        return Ok(());
                    }
                    if inner.path.is_ident("list_filter") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.list_filter =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    if inner.path.is_ident("actions") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.actions =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    if inner.path.is_ident("fieldsets") {
                        let s: LitStr = inner.value()?.parse()?;
                        admin.fieldsets =
                            Some((parse_fieldset_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown admin attribute (supported: \
                         `list_display`, `search_fields`, `readonly_fields`, \
                         `list_filter`, `list_per_page`, `ordering`, `actions`, \
                         `fieldsets`)",
                    ))
                })?;
                out.admin = Some(admin);
                return Ok(());
            }
            if meta.path.is_ident("manager") {
                // `#[rustango(manager(ext = "FooManagerExt"))]`. Issue #271 / T1.9.
                // Stretch `from_queryset = "..."` (Django Manager.from_queryset
                // shape) is left as a follow-up — the issue's primary
                // acceptance is the `ext = ...` trait emission.
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("ext") {
                        let s: LitStr = inner.value()?.parse()?;
                        let name = s.value();
                        if name.is_empty() {
                            return Err(inner.error("manager(ext = \"...\") cannot be empty"));
                        }
                        out.manager_ext =
                            Some(syn::Ident::new(&name, s.span()));
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown manager attribute (supported: `ext = \"TraitName\"`)",
                    ))
                })?;
                return Ok(());
            }
            if meta.path.is_ident("manager_fn") {
                // `#[rustango(manager_fn = "active")]` — issue #289 / T2.6.
                // Adds a `pub fn <name>() -> QuerySet<Self>` accessor
                // next to the default `Self::objects()`. Multiple
                // attributes accumulate.
                let s: LitStr = meta.value()?.parse()?;
                let name = s.value();
                if name.is_empty() {
                    return Err(meta.error("`manager_fn = \"...\"` cannot be empty"));
                }
                if name == "objects" {
                    return Err(meta.error(
                        "`manager_fn = \"objects\"` collides with the default \
                         accessor — pick a different name",
                    ));
                }
                let ident = syn::Ident::new(&name, s.span());
                if out.manager_fns.iter().any(|prev| *prev == ident) {
                    return Err(meta.error(format!(
                        "duplicate `manager_fn = \"{name}\"`"
                    )));
                }
                out.manager_fns.push(ident);
                return Ok(());
            }
            if meta.path.is_ident("default_order") {
                // `#[rustango(default_order = "-created_at, status")]`
                // — issue #291 / T2.5. Comma-separated list; `-prefix`
                // means descending, `+prefix` or bare name means ascending.
                // Per-query opt-in via `QuerySet::with_default_order()`.
                let s: LitStr = meta.value()?.parse()?;
                let raw = s.value();
                let span = s.span();
                let mut parsed: Vec<(String, bool, proc_macro2::Span)> =
                    Vec::new();
                for entry in raw.split(',') {
                    let trimmed = entry.trim();
                    if trimmed.is_empty() {
                        return Err(syn::Error::new(
                            span,
                            "`default_order = \"...\"` has an empty entry — \
                             check for a stray comma",
                        ));
                    }
                    let (desc, name) = if let Some(rest) = trimmed.strip_prefix('-') {
                        (true, rest.trim().to_owned())
                    } else if let Some(rest) = trimmed.strip_prefix('+') {
                        (false, rest.trim().to_owned())
                    } else {
                        (false, trimmed.to_owned())
                    };
                    if name.is_empty() {
                        return Err(syn::Error::new(
                            span,
                            "`default_order` entry has no column name after the prefix",
                        ));
                    }
                    if parsed.iter().any(|(n, _, _)| *n == name) {
                        return Err(syn::Error::new(
                            span,
                            format!("duplicate column `{name}` in `default_order`"),
                        ));
                    }
                    parsed.push((name, desc, span));
                }
                if parsed.is_empty() {
                    return Err(syn::Error::new(
                        span,
                        "`default_order = \"...\"` cannot be empty",
                    ));
                }
                out.default_order = parsed;
                return Ok(());
            }
            if meta.path.is_ident("audit") {
                let mut audit = AuditAttrs::default();
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("track") {
                        let s: LitStr = inner.value()?.parse()?;
                        audit.track =
                            Some((split_field_list(&s.value()), s.span()));
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown audit attribute (supported: `track`)",
                    ))
                })?;
                out.audit = Some(audit);
                return Ok(());
            }
            if meta.path.is_ident("permissions") {
                // Two forms accepted:
                //   #[rustango(permissions)]          — flag form, true
                //   #[rustango(permissions = false)]  — explicit opt-out
                //   #[rustango(permissions = true)]   — explicit opt-in
                if let Ok(v) = meta.value() {
                    let lit: syn::LitBool = v.parse()?;
                    out.permissions = lit.value;
                } else {
                    out.permissions = true;
                }
                return Ok(());
            }
            if meta.path.is_ident("view") {
                // Issue #293 / T2.10. Two forms accepted, matching
                // the `permissions` flag pattern:
                //   #[rustango(view)]          — flag form, true
                //   #[rustango(view = false)]  — explicit opt-out
                //   #[rustango(view = true)]   — explicit opt-in
                if let Ok(v) = meta.value() {
                    let lit: syn::LitBool = v.parse()?;
                    out.is_view = lit.value;
                } else {
                    out.is_view = true;
                }
                return Ok(());
            }
            if meta.path.is_ident("verbose_name") {
                let s: LitStr = meta.value()?.parse()?;
                out.verbose_name = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("verbose_name_plural") {
                let s: LitStr = meta.value()?.parse()?;
                out.verbose_name_plural = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("unique_together") {
                // Django-shape composite UNIQUE index. Two syntaxes:
                //
                //   #[rustango(unique_together = "org_id, user_id")]                       — auto-derived name
                //   #[rustango(unique_together(columns = "org_id, user_id", name = "x"))]  — explicit name
                //
                // Both produce `CREATE UNIQUE INDEX <name> ON <table>
                // (col1, col2)`, where <name> defaults to
                // `<table>_<col1>_<col2>_uq` when not supplied.
                let (columns, name) = parse_together_attr(&meta, "unique_together")?;
                out.indexes.push(IndexAttr {
                    name,
                    columns,
                    unique: true,
                    method: "btree".to_owned(),
                    where_clause: None,
                });
                return Ok(());
            }
            if meta.path.is_ident("index_together") {
                // Django-shape composite (non-unique) index. Two syntaxes
                // mirroring `unique_together`.
                //
                //   #[rustango(index_together = "created_at, status")]
                //   #[rustango(index_together(columns = "created_at, status", name = "x"))]
                let (columns, name) = parse_together_attr(&meta, "index_together")?;
                out.indexes.push(IndexAttr {
                    name,
                    columns,
                    unique: false,
                    method: "btree".to_owned(),
                    where_clause: None,
                });
                return Ok(());
            }
            if meta.path.is_ident("unique_when") {
                // Django 4.0+ `UniqueConstraint(condition=Q(...))` —
                // partial unique index. Issue #265 / T1.3.
                //
                //   #[rustango(unique_when(
                //       columns   = "email",
                //       condition = "deleted_at IS NULL",
                //       name      = "unique_active_email"
                //   ))]
                //
                // → `CREATE UNIQUE INDEX <name> ON <table> (cols) WHERE <condition>`
                // on PG / SQLite (both ship partial indexes natively).
                // MySQL falls back to a plain UNIQUE index — the
                // condition is lost; document the limitation in the
                // generated migration.
                let mut columns: Option<Vec<String>> = None;
                let mut condition: Option<String> = None;
                let mut name: Option<String> = None;
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("columns") {
                        let s: LitStr = inner.value()?.parse()?;
                        columns = Some(split_field_list(&s.value()));
                        return Ok(());
                    }
                    if inner.path.is_ident("condition") {
                        let s: LitStr = inner.value()?.parse()?;
                        condition = Some(s.value());
                        return Ok(());
                    }
                    if inner.path.is_ident("name") {
                        let s: LitStr = inner.value()?.parse()?;
                        name = Some(s.value());
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown unique_when attribute (supported: \
                         `columns = \"...\"`, `condition = \"...\"`, \
                         `name = \"...\"`)",
                    ))
                })?;
                let columns = columns.ok_or_else(|| {
                    meta.error("`unique_when(...)` requires `columns = \"...\"`")
                })?;
                let condition = condition.ok_or_else(|| {
                    meta.error("`unique_when(...)` requires `condition = \"...\"`")
                })?;
                if columns.is_empty() {
                    return Err(meta.error("`unique_when(columns = \"\")` is empty"));
                }
                out.indexes.push(IndexAttr {
                    name,
                    columns,
                    unique: true,
                    method: "btree".to_owned(),
                    where_clause: Some(condition),
                });
                return Ok(());
            }
            if meta.path.is_ident("index") {
                // Container-level composite index — legacy entry that
                // was advertised with a trailing `, unique, name = ...`
                // flag block which doesn't actually compose under
                // `parse_nested_meta`. Prefer `unique_together` /
                // `index_together` (above) for new code. The bare
                // `index = "..."` form is kept for back-compat: it
                // emits a non-unique composite index.
                let cols_lit: LitStr = meta.value()?.parse()?;
                let columns = split_field_list(&cols_lit.value());
                out.indexes.push(IndexAttr {
                    name: None,
                    columns,
                    unique: false,
                    method: "btree".to_owned(),
                    where_clause: None,
                });
                return Ok(());
            }
            if meta.path.is_ident("check") {
                // #[rustango(check(name = "…", expr = "…"))]
                let mut name: Option<String> = None;
                let mut expr: Option<String> = None;
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("name") {
                        let s: LitStr = inner.value()?.parse()?;
                        name = Some(s.value());
                        return Ok(());
                    }
                    if inner.path.is_ident("expr") {
                        let s: LitStr = inner.value()?.parse()?;
                        expr = Some(s.value());
                        return Ok(());
                    }
                    Err(inner.error("unknown check attribute (supported: `name`, `expr`)"))
                })?;
                let name = name.ok_or_else(|| meta.error("check requires `name = \"...\"`"))?;
                let expr = expr.ok_or_else(|| meta.error("check requires `expr = \"...\"`"))?;
                out.checks.push(CheckAttr { name, expr });
                return Ok(());
            }
            if meta.path.is_ident("generic_fk") {
                let mut gfk = GenericFkAttr {
                    name: String::new(),
                    ct_column: String::new(),
                    pk_column: String::new(),
                };
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("name") {
                        let s: LitStr = inner.value()?.parse()?;
                        gfk.name = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("ct_column") {
                        let s: LitStr = inner.value()?.parse()?;
                        gfk.ct_column = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("pk_column") {
                        let s: LitStr = inner.value()?.parse()?;
                        gfk.pk_column = s.value();
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown generic_fk attribute (supported: `name`, `ct_column`, `pk_column`)",
                    ))
                })?;
                if gfk.name.is_empty() {
                    return Err(meta.error("generic_fk requires `name = \"...\"`"));
                }
                if gfk.ct_column.is_empty() {
                    return Err(meta.error("generic_fk requires `ct_column = \"...\"`"));
                }
                if gfk.pk_column.is_empty() {
                    return Err(meta.error("generic_fk requires `pk_column = \"...\"`"));
                }
                out.generic_fks.push(gfk);
                return Ok(());
            }
            if meta.path.is_ident("fk_composite") {
                let mut fk = CompositeFkAttr {
                    name: String::new(),
                    to: String::new(),
                    from: Vec::new(),
                    on: Vec::new(),
                };
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("name") {
                        let s: LitStr = inner.value()?.parse()?;
                        fk.name = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("to") {
                        let s: LitStr = inner.value()?.parse()?;
                        fk.to = s.value();
                        return Ok(());
                    }
                    // `on = ("col1", "col2", ...)` — parse a parenthesised
                    // comma-list of string literals.
                    if inner.path.is_ident("on") || inner.path.is_ident("from") {
                        let value = inner.value()?;
                        let content;
                        syn::parenthesized!(content in value);
                        let lits: syn::punctuated::Punctuated<syn::LitStr, syn::Token![,]> =
                            content.parse_terminated(
                                |p| p.parse::<syn::LitStr>(),
                                syn::Token![,],
                            )?;
                        let cols: Vec<String> = lits.iter().map(syn::LitStr::value).collect();
                        if inner.path.is_ident("on") {
                            fk.on = cols;
                        } else {
                            fk.from = cols;
                        }
                        return Ok(());
                    }
                    Err(inner.error(
                        "unknown fk_composite attribute (supported: `name`, `to`, `on`, `from`)",
                    ))
                })?;
                if fk.name.is_empty() {
                    return Err(meta.error("fk_composite requires `name = \"...\"`"));
                }
                if fk.to.is_empty() {
                    return Err(meta.error("fk_composite requires `to = \"...\"`"));
                }
                if fk.from.is_empty() || fk.on.is_empty() {
                    return Err(meta.error(
                        "fk_composite requires non-empty `from = (...)` and `on = (...)` tuples",
                    ));
                }
                if fk.from.len() != fk.on.len() {
                    return Err(meta.error(format!(
                        "fk_composite `from` ({} cols) and `on` ({} cols) must be the same length",
                        fk.from.len(),
                        fk.on.len(),
                    )));
                }
                out.composite_fks.push(fk);
                return Ok(());
            }
            if meta.path.is_ident("m2m") {
                let mut m2m = M2MAttr {
                    name: String::new(),
                    to: String::new(),
                    through: String::new(),
                    src: String::new(),
                    dst: String::new(),
                };
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("name") {
                        let s: LitStr = inner.value()?.parse()?;
                        m2m.name = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("to") {
                        let s: LitStr = inner.value()?.parse()?;
                        m2m.to = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("through") {
                        let s: LitStr = inner.value()?.parse()?;
                        m2m.through = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("src") {
                        let s: LitStr = inner.value()?.parse()?;
                        m2m.src = s.value();
                        return Ok(());
                    }
                    if inner.path.is_ident("dst") {
                        let s: LitStr = inner.value()?.parse()?;
                        m2m.dst = s.value();
                        return Ok(());
                    }
                    Err(inner.error("unknown m2m attribute (supported: `name`, `to`, `through`, `src`, `dst`)"))
                })?;
                if m2m.name.is_empty() {
                    return Err(meta.error("m2m requires `name = \"...\"`"));
                }
                if m2m.to.is_empty() {
                    return Err(meta.error("m2m requires `to = \"...\"`"));
                }
                if m2m.through.is_empty() {
                    return Err(meta.error("m2m requires `through = \"...\"`"));
                }
                if m2m.src.is_empty() {
                    return Err(meta.error("m2m requires `src = \"...\"`"));
                }
                if m2m.dst.is_empty() {
                    return Err(meta.error("m2m requires `dst = \"...\"`"));
                }
                out.m2m.push(m2m);
                return Ok(());
            }
            Err(meta.error("unknown rustango container attribute"))
        })?;
    }
    Ok(out)
}

/// Split a comma-separated field-name list (e.g. `"name, office"`) into
/// owned field names, trimming whitespace and skipping empty entries.
/// Field-name validation against the model is done by the caller.
fn split_field_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Shared parser for `unique_together` and `index_together` container
/// attrs. Accepts both shapes:
///
///   * `attr = "col1, col2"`              — auto-derived index name.
///   * `attr(columns = "col1, col2", name = "...")` — explicit name.
///
/// Returns `(columns, name)`.
fn parse_together_attr(
    meta: &syn::meta::ParseNestedMeta<'_>,
    attr: &str,
) -> syn::Result<(Vec<String>, Option<String>)> {
    // Disambiguate by whether the next token is `=` (key-value) or
    // `(` (parenthesized).
    if meta.input.peek(syn::Token![=]) {
        let cols_lit: LitStr = meta.value()?.parse()?;
        let columns = split_field_list(&cols_lit.value());
        check_together_columns(meta, attr, &columns)?;
        return Ok((columns, None));
    }
    let mut columns: Option<Vec<String>> = None;
    let mut name: Option<String> = None;
    meta.parse_nested_meta(|inner| {
        if inner.path.is_ident("columns") {
            let s: LitStr = inner.value()?.parse()?;
            columns = Some(split_field_list(&s.value()));
            return Ok(());
        }
        if inner.path.is_ident("name") {
            let s: LitStr = inner.value()?.parse()?;
            name = Some(s.value());
            return Ok(());
        }
        Err(inner.error("unknown sub-attribute (supported: `columns`, `name`)"))
    })?;
    let columns = columns.ok_or_else(|| {
        meta.error(format!(
            "{attr}(...) requires a `columns = \"col1, col2\"` argument",
        ))
    })?;
    check_together_columns(meta, attr, &columns)?;
    Ok((columns, name))
}

fn check_together_columns(
    meta: &syn::meta::ParseNestedMeta<'_>,
    attr: &str,
    columns: &[String],
) -> syn::Result<()> {
    if columns.len() < 2 {
        let single = if attr == "unique_together" {
            "#[rustango(unique)] on the field"
        } else {
            "#[rustango(index)] on the field"
        };
        return Err(meta.error(format!(
            "{attr} expects two or more columns; for a single-column equivalent use {single}",
        )));
    }
    Ok(())
}

/// Parse the fieldsets DSL: pipe-separated sections, optional
/// `"Title:"` prefix on each, comma-separated field names after.
/// Examples:
/// * `"name, office"` → one untitled section with two fields
/// * `"Identity: name, office | Metadata: created_at"` → two titled
///   sections
///
/// Returns `(title, fields)` pairs. Title is `""` when no prefix.
fn parse_fieldset_list(raw: &str) -> Vec<(String, Vec<String>)> {
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|section| {
            // Split off an optional `Title:` prefix (first colon).
            let (title, rest) = match section.split_once(':') {
                Some((title, rest)) if !title.contains(',') => (title.trim().to_owned(), rest),
                _ => (String::new(), section),
            };
            let fields = split_field_list(rest);
            (title, fields)
        })
        .collect()
}

/// Parse Django-shape ordering — `"name"` is ASC, `"-name"` is DESC.
/// Returns `(field_name, desc)` pairs in the same order as the input.
fn parse_ordering_list(raw: &str) -> Vec<(String, bool)> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|spec| {
            spec.strip_prefix('-')
                .map_or((spec.to_owned(), false), |rest| {
                    (rest.trim().to_owned(), true)
                })
        })
        .collect()
}

struct FieldAttrs {
    column: Option<String>,
    primary_key: bool,
    fk: Option<String>,
    o2o: Option<String>,
    on: Option<String>,
    max_length: Option<u32>,
    min: Option<i64>,
    max: Option<i64>,
    default: Option<String>,
    /// `#[rustango(auto_uuid)]` — UUID PK generated by Postgres
    /// `gen_random_uuid()`. Implies `auto + primary_key + default =
    /// "gen_random_uuid()"`. The Rust field type must be
    /// `uuid::Uuid` (or `Auto<Uuid>`); the column is excluded from
    /// INSERTs so the DB DEFAULT fires.
    auto_uuid: bool,
    /// `#[rustango(auto_now_add)]` — `created_at`-shape column.
    /// Server-set on insert, immutable from app code afterwards.
    /// Implies `auto + default = "now()"`. Field type must be
    /// `DateTime<Utc>`.
    auto_now_add: bool,
    /// `#[rustango(auto_now)]` — `updated_at`-shape column. Set on
    /// every insert AND every update. Implies `auto + default =
    /// "now()"`; the macro additionally rewrites `update_on` /
    /// `save_on` to bind `chrono::Utc::now()` instead of the user's
    /// field value.
    auto_now: bool,
    /// `#[rustango(soft_delete)]` — `deleted_at`-shape column. Type
    /// must be `Option<DateTime<Utc>>`. Triggers macro emission of
    /// `soft_delete_on(executor)` and `restore_on(executor)`
    /// methods on the model.
    soft_delete: bool,
    /// `#[rustango(unique)]` — adds a `UNIQUE` constraint inline on
    /// the column in the generated DDL.
    unique: bool,
    /// `#[rustango(index)]` or `#[rustango(index(name = "…", unique))]` —
    /// generates a `CREATE INDEX` for this column. `unique` here means
    /// `CREATE UNIQUE INDEX` (distinct from the `unique` constraint above).
    index: bool,
    index_unique: bool,
    index_name: Option<String>,
    /// Index access method (`"btree"` / `"gin"` / …). Defaults to
    /// `"btree"`. Issue #34.
    index_method: String,
    /// `#[rustango(generated_as = "EXPR")]` — emit `GENERATED ALWAYS
    /// AS (EXPR) STORED` in the column DDL. Read-only from app code:
    /// the macro skips this column from every INSERT and UPDATE
    /// path, so the database always recomputes the value from
    /// `EXPR`. Backlog item #35.
    generated_as: Option<String>,
    /// `#[rustango(help_text = "…")]` — Django-shape help text
    /// rendered below the admin form's input. Threaded into
    /// `FieldSchema::help_text` so admin / serializer / OpenAPI
    /// layers can read it.
    help_text: Option<String>,
    /// `#[rustango(choices = "value:Label, value:Label")]` — Django-shape
    /// enumerated allowed values. Threaded into `FieldSchema::choices`
    /// as a `&'static [(&'static str, &'static str)]` slice. When
    /// present, the admin form renders a `<select>` instead of `<input>`
    /// and the validator rejects values not in the list. Only meaningful
    /// for `FieldType::String`; the macro errors at compile time if
    /// applied to a non-string field.
    choices: Option<Vec<(String, String)>>,
    /// `#[rustango(db_comment = "…")]` — Django-shape DB-side column
    /// comment. Threaded into `FieldSchema::db_comment`. MySQL inlines
    /// the comment in CREATE TABLE; Postgres emits a separate
    /// `COMMENT ON COLUMN` statement after the table is created;
    /// SQLite silently drops the value (no native column comments).
    db_comment: Option<String>,
    /// `#[rustango(verbose_name = "…")]` — Django-shape human-readable
    /// label for the field. Threaded into `FieldSchema::verbose_name`
    /// so admin column headers, form labels, and other display
    /// surfaces can prefer the friendly caption over the Rust
    /// identifier. `None` means renderers fall back to the field name.
    verbose_name: Option<String>,
    /// `#[rustango(editable = false)]` — Django-shape opt-out from
    /// auto-generated form rendering. Defaults to `true` so existing
    /// fields keep their current admin / form behavior; setting
    /// `false` removes the field from the admin change-form entirely
    /// (the value is still visible on detail / list views, just not
    /// editable).
    editable: bool,
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs {
        column: None,
        primary_key: false,
        fk: None,
        o2o: None,
        on: None,
        max_length: None,
        min: None,
        max: None,
        default: None,
        auto_uuid: false,
        auto_now_add: false,
        auto_now: false,
        soft_delete: false,
        unique: false,
        index: false,
        index_unique: false,
        index_name: None,
        index_method: "btree".to_owned(),
        generated_as: None,
        help_text: None,
        choices: None,
        db_comment: None,
        verbose_name: None,
        editable: true,
    };
    for attr in &field.attrs {
        if !attr.path().is_ident("rustango") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("column") {
                let s: LitStr = meta.value()?.parse()?;
                let name = s.value();
                validate_sql_identifier(&name, "column", s.span())?;
                out.column = Some(name);
                return Ok(());
            }
            if meta.path.is_ident("primary_key") {
                out.primary_key = true;
                return Ok(());
            }
            if meta.path.is_ident("fk") {
                let s: LitStr = meta.value()?.parse()?;
                out.fk = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("o2o") {
                let s: LitStr = meta.value()?.parse()?;
                out.o2o = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("on") {
                let s: LitStr = meta.value()?.parse()?;
                out.on = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("max_length") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                out.max_length = Some(lit.base10_parse::<u32>()?);
                return Ok(());
            }
            if meta.path.is_ident("min") {
                out.min = Some(parse_signed_i64(&meta)?);
                return Ok(());
            }
            if meta.path.is_ident("max") {
                out.max = Some(parse_signed_i64(&meta)?);
                return Ok(());
            }
            if meta.path.is_ident("default") {
                let s: LitStr = meta.value()?.parse()?;
                out.default = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("generated_as") {
                let s: LitStr = meta.value()?.parse()?;
                out.generated_as = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("help_text") {
                let s: LitStr = meta.value()?.parse()?;
                out.help_text = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("choices") {
                let s: LitStr = meta.value()?.parse()?;
                let raw = s.value();
                let mut pairs: Vec<(String, String)> = Vec::new();
                for chunk in raw.split(',') {
                    let chunk = chunk.trim();
                    if chunk.is_empty() {
                        continue;
                    }
                    let (value, label) = match chunk.split_once(':') {
                        Some((v, l)) => (v.trim().to_owned(), l.trim().to_owned()),
                        None => (chunk.to_owned(), chunk.to_owned()),
                    };
                    if value.is_empty() {
                        return Err(syn::Error::new(
                            s.span(),
                            "`choices` entry has empty value before `:`",
                        ));
                    }
                    pairs.push((value, label));
                }
                if pairs.is_empty() {
                    return Err(syn::Error::new(
                        s.span(),
                        "`choices = \"…\"` must contain at least one value",
                    ));
                }
                out.choices = Some(pairs);
                return Ok(());
            }
            if meta.path.is_ident("db_comment") {
                let s: LitStr = meta.value()?.parse()?;
                out.db_comment = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("verbose_name") {
                let s: LitStr = meta.value()?.parse()?;
                out.verbose_name = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("editable") {
                // Two forms accepted:
                //   #[rustango(editable = false)] / true — explicit
                //   #[rustango(editable)] — flag form (= true, the
                //   default, so harmless; included for symmetry)
                if let Ok(v) = meta.value() {
                    let lit: syn::LitBool = v.parse()?;
                    out.editable = lit.value;
                } else {
                    out.editable = true;
                }
                return Ok(());
            }
            if meta.path.is_ident("auto_uuid") {
                out.auto_uuid = true;
                // Implied: PK + auto + DEFAULT gen_random_uuid().
                // Each is also explicitly settable; the explicit
                // value wins if conflicting.
                out.primary_key = true;
                if out.default.is_none() {
                    out.default = Some("gen_random_uuid()".into());
                }
                return Ok(());
            }
            if meta.path.is_ident("auto_now_add") {
                out.auto_now_add = true;
                if out.default.is_none() {
                    out.default = Some("now()".into());
                }
                return Ok(());
            }
            if meta.path.is_ident("auto_now") {
                out.auto_now = true;
                if out.default.is_none() {
                    out.default = Some("now()".into());
                }
                return Ok(());
            }
            if meta.path.is_ident("soft_delete") {
                out.soft_delete = true;
                return Ok(());
            }
            if meta.path.is_ident("unique") {
                out.unique = true;
                return Ok(());
            }
            if meta.path.is_ident("index") {
                out.index = true;
                // Optional sub-attrs: #[rustango(index(unique, name = "…", method = "gin"))]
                if meta.input.peek(syn::token::Paren) {
                    meta.parse_nested_meta(|inner| {
                        if inner.path.is_ident("unique") {
                            out.index_unique = true;
                            return Ok(());
                        }
                        if inner.path.is_ident("name") {
                            let s: LitStr = inner.value()?.parse()?;
                            out.index_name = Some(s.value());
                            return Ok(());
                        }
                        if inner.path.is_ident("method") {
                            let s: LitStr = inner.value()?.parse()?;
                            let v = s.value();
                            match v.as_str() {
                                "btree" | "gin" | "gist" | "brin" | "spgist" | "hash" | "bloom" => {
                                    out.index_method = v;
                                }
                                other => {
                                    return Err(inner.error(format!(
                                        "unknown index method `{other}` (supported: btree, gin, gist, brin, spgist, hash, bloom)",
                                    )));
                                }
                            }
                            return Ok(());
                        }
                        Err(inner.error(
                            "unknown index sub-attribute (supported: `unique`, `name`, `method`)",
                        ))
                    })?;
                }
                return Ok(());
            }
            Err(meta.error("unknown rustango field attribute"))
        })?;
    }
    Ok(out)
}

/// Parse a signed integer literal, accepting optional leading `-`.
fn parse_signed_i64(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<i64> {
    let expr: syn::Expr = meta.value()?.parse()?;
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => lit.base10_parse::<i64>(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(lit),
                ..
            }) = *expr
            {
                let v: i64 = lit.base10_parse()?;
                Ok(-v)
            } else {
                Err(syn::Error::new_spanned(expr, "expected integer literal"))
            }
        }
        other => Err(syn::Error::new_spanned(
            other,
            "expected integer literal (signed)",
        )),
    }
}

struct FieldInfo<'a> {
    ident: &'a syn::Ident,
    column: String,
    primary_key: bool,
    /// `true` when the Rust type was `Auto<T>` — the INSERT path will
    /// skip this column when `Auto::Unset` and emit it under
    /// `RETURNING` so Postgres' sequence DEFAULT fills in the value.
    auto: bool,
    /// The original field type, e.g. `i64` or `Option<String>`. Emitted as
    /// the `Column::Value` associated type for typed-column tokens.
    value_ty: &'a Type,
    /// `FieldType` variant tokens (`::rustango::core::FieldType::I64`).
    field_type_tokens: TokenStream2,
    schema: TokenStream2,
    from_row_init: TokenStream2,
    /// Variant of [`Self::from_row_init`] that reads the column via
    /// `format!("{prefix}__{col}")` so a model can be decoded out of
    /// the aliased columns of a JOINed row. Drives slice 9.0d's
    /// `Self::__rustango_from_aliased_row(row, prefix)` per-Model
    /// helper that `select_related` calls when stitching loaded FKs.
    from_aliased_row_init: TokenStream2,
    /// Inner type from a `ForeignKey<T, K>` field, if any. The reverse-
    /// relation helper emit (`Author::<child>_set`) needs to know `T`
    /// to point the generated method at the right child model.
    fk_inner: Option<Type>,
    /// `K`'s scalar kind for a `ForeignKey<T, K>` field. Mirrors
    /// `kind` (since ForeignKey detection sets `kind` to K's
    /// underlying type) but stored separately for clarity at the
    /// `FkRelation` construction site, which only sees the FK's
    /// surface fields.
    fk_pk_kind: DetectedKind,
    /// `true` when the field is `Option<ForeignKey<T, K>>` rather than
    /// the bare `ForeignKey<T, K>`. Routes the load_related and
    /// fk_pk_access emitters to wrap assignments / accessors in
    /// `Some(...)` / `as_ref().map(...)` respectively, so a nullable
    /// FK column compiles end-to-end. The DDL writer reads this off
    /// the field schema (`nullable` flag); the macro just needs to
    /// keep the Rust-side codegen consistent.
    nullable: bool,
    /// `true` when this column was marked `#[rustango(auto_now)]` —
    /// `update_on` / `save_on` bind `chrono::Utc::now()` for this
    /// column instead of the user-supplied value, so `updated_at`
    /// always reflects the latest write without the caller having
    /// to remember to set it.
    auto_now: bool,
    /// `true` when this column was marked `#[rustango(auto_now_add)]`
    /// — the column is server-set on INSERT (DB DEFAULT) and
    /// **immutable** afterwards. `update_on` / `save_on` skip the
    /// column entirely so a stale `created_at` value in memory never
    /// rewrites the persisted timestamp.
    auto_now_add: bool,
    /// `true` when this column was marked `#[rustango(soft_delete)]`.
    /// Triggers emission of `soft_delete_on(executor)` and
    /// `restore_on(executor)` on the model's inherent impl. There is
    /// at most one such column per model — emission asserts this.
    soft_delete: bool,
    /// `Some` when this column was marked
    /// `#[rustango(generated_as = "EXPR")]`. The macro skips it from
    /// every INSERT and UPDATE path; the database recomputes the
    /// value from `EXPR`. Backlog item #35.
    generated_as: Option<String>,
}

/// Reject table names that won't survive SQL identifier
/// derivation downstream. Postgres' regular-identifier rule
/// (`[a-zA-Z_][a-zA-Z0-9_]*`) is the safe shape: it round-trips
/// through the framework's unquoted FK / index / constraint name
/// emission without surprises. We also disallow leading-digit and
/// the empty string for clarity.
///
/// Reserved-word collisions (`select`, `from`, …) aren't flagged
/// here — those produce a runtime error from the SQL parser,
/// which is loud enough; statically enumerating reserved words
/// across the three supported dialects is more friction than help.
///
/// Backlog item #65.
fn validate_table_name(name: &str, span: proc_macro2::Span) -> syn::Result<()> {
    validate_sql_identifier(name, "table", span)
}

/// Reject SQL identifiers that compile but break downstream SQL
/// generation. Same rule for tables and columns: `[a-zA-Z_][a-zA-Z0-9_]*`.
/// `kind` is "table" / "column" — used for the error message so users
/// see which attribute caused the failure.
fn validate_sql_identifier(name: &str, kind: &str, span: proc_macro2::Span) -> syn::Result<()> {
    if name.is_empty() {
        return Err(syn::Error::new(
            span,
            format!("`{kind} = \"\"` is not a valid SQL identifier"),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(syn::Error::new(
            span,
            format!("{kind} name `{name}` must start with a letter or underscore (got {first:?})"),
        ));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(syn::Error::new(
                span,
                format!(
                    "{kind} name `{name}` contains invalid character {c:?} — \
                     SQL identifiers must match `[a-zA-Z_][a-zA-Z0-9_]*`. \
                     Hyphens in particular break FK / index name derivation \
                     downstream; use underscores instead (e.g. `{}`)",
                    name.replace(|x: char| !x.is_ascii_alphanumeric() && x != '_', "_"),
                ),
            ));
        }
    }
    Ok(())
}

fn process_field<'a>(field: &'a syn::Field, table: &str) -> syn::Result<FieldInfo<'a>> {
    let attrs = parse_field_attrs(field)?;
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new(field.span(), "tuple structs are not supported"))?;
    let name = ident.to_string();
    let column = attrs.column.clone().unwrap_or_else(|| name.clone());
    let primary_key = attrs.primary_key;
    let DetectedType {
        kind,
        nullable,
        auto: detected_auto,
        fk_inner,
    } = detect_type(&field.ty)?;
    check_bound_compatibility(field, &attrs, kind)?;
    let auto = detected_auto;
    // Mixin attributes piggyback on the existing `Auto<T>` skip-on-
    // INSERT path: the user must wrap the field in `Auto<T>`, which
    // marks the column as DB-default-supplied. The mixin attrs then
    // layer in the SQL default (`now()` / `gen_random_uuid()`) and,
    // for `auto_now`, force the value on UPDATE too.
    if attrs.auto_uuid {
        if kind != DetectedKind::Uuid {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(auto_uuid)]` requires the field type to be \
                 `Auto<uuid::Uuid>`",
            ));
        }
        if !detected_auto {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(auto_uuid)]` requires the field type to be \
                 wrapped in `Auto<...>` so the macro skips the column on \
                 INSERT and the DB DEFAULT (`gen_random_uuid()`) fires",
            ));
        }
    }
    if attrs.auto_now_add || attrs.auto_now {
        if kind != DetectedKind::DateTime {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(auto_now_add)]` / `#[rustango(auto_now)]` require \
                 the field type to be `Auto<chrono::DateTime<chrono::Utc>>`",
            ));
        }
        if !detected_auto {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(auto_now_add)]` / `#[rustango(auto_now)]` require \
                 the field type to be wrapped in `Auto<...>` so the macro skips \
                 the column on INSERT and the DB DEFAULT (`now()`) fires",
            ));
        }
    }
    if attrs.soft_delete && !(kind == DetectedKind::DateTime && nullable) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[rustango(soft_delete)]` requires the field type to be \
             `Option<chrono::DateTime<chrono::Utc>>`",
        ));
    }
    let is_mixin_auto = attrs.auto_uuid || attrs.auto_now_add || attrs.auto_now;
    if detected_auto && !primary_key && !is_mixin_auto {
        return Err(syn::Error::new_spanned(
            field,
            "`Auto<T>` is only valid on a `#[rustango(primary_key)]` field, \
             or on a field carrying one of `auto_uuid`, `auto_now_add`, or \
             `auto_now`",
        ));
    }
    if detected_auto && attrs.default.is_some() && !is_mixin_auto {
        return Err(syn::Error::new_spanned(
            field,
            "`#[rustango(default = \"…\")]` is redundant on an `Auto<T>` field — \
             SERIAL / BIGSERIAL already supplies a default sequence.",
        ));
    }
    if fk_inner.is_some() && primary_key {
        return Err(syn::Error::new_spanned(
            field,
            "`ForeignKey<T>` is not allowed on a primary-key field — \
             a row's PK is its own identity, not a reference to a parent.",
        ));
    }
    if attrs.generated_as.is_some() {
        if primary_key {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(generated_as = \"…\")]` is not allowed on a \
                 primary-key field — a PK must be writable so the row \
                 has an identity at INSERT time.",
            ));
        }
        if attrs.default.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(generated_as = \"…\")]` cannot combine with \
                 `default = \"…\"` — Postgres rejects DEFAULT on \
                 generated columns. The expression IS the default.",
            ));
        }
        if detected_auto {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(generated_as = \"…\")]` is not allowed on \
                 an `Auto<T>` field — generated columns are computed \
                 by the DB, not server-assigned via a sequence. Use a \
                 plain Rust type (e.g. `f64`).",
            ));
        }
        if fk_inner.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "`#[rustango(generated_as = \"…\")]` is not allowed on a \
                 ForeignKey field.",
            ));
        }
    }
    let relation = relation_tokens(field, &attrs, fk_inner, table)?;
    let column_lit = column.as_str();
    let field_type_tokens = kind.variant_tokens();
    let max_length = optional_u32(attrs.max_length);
    let min = optional_i64(attrs.min);
    let max = optional_i64(attrs.max);
    let default = optional_str(attrs.default.as_deref());

    let unique = attrs.unique;
    let generated_as = optional_str(attrs.generated_as.as_deref());
    let help_text = optional_str(attrs.help_text.as_deref());
    let choices = optional_choices(attrs.choices.as_deref());
    let db_comment = optional_str(attrs.db_comment.as_deref());
    let verbose_name = optional_str(attrs.verbose_name.as_deref());
    let editable = attrs.editable;
    let schema = quote! {
        ::rustango::core::FieldSchema {
            name: #name,
            column: #column_lit,
            ty: #field_type_tokens,
            nullable: #nullable,
            primary_key: #primary_key,
            relation: #relation,
            max_length: #max_length,
            min: #min,
            max: #max,
            default: #default,
            auto: #auto,
            unique: #unique,
            generated_as: #generated_as,
            help_text: #help_text,
            choices: #choices,
            db_comment: #db_comment,
            verbose_name: #verbose_name,
            editable: #editable,
        }
    };

    let from_row_init = quote! {
        #ident: ::rustango::sql::sqlx::Row::try_get(row, #column_lit)?
    };
    let from_aliased_row_init = quote! {
        #ident: ::rustango::sql::sqlx::Row::try_get(
            row,
            ::std::format!("{}__{}", prefix, #column_lit).as_str(),
        )?
    };

    Ok(FieldInfo {
        ident,
        column,
        primary_key,
        auto,
        value_ty: &field.ty,
        field_type_tokens,
        schema,
        from_row_init,
        from_aliased_row_init,
        fk_inner: fk_inner.cloned(),
        fk_pk_kind: kind,
        nullable,
        auto_now: attrs.auto_now,
        auto_now_add: attrs.auto_now_add,
        soft_delete: attrs.soft_delete,
        generated_as: attrs.generated_as.clone(),
    })
}

fn check_bound_compatibility(
    field: &syn::Field,
    attrs: &FieldAttrs,
    kind: DetectedKind,
) -> syn::Result<()> {
    if attrs.max_length.is_some() && kind != DetectedKind::String {
        return Err(syn::Error::new_spanned(
            field,
            "`max_length` is only valid on `String` fields (or `Option<String>`)",
        ));
    }
    if attrs.choices.is_some() && kind != DetectedKind::String {
        return Err(syn::Error::new_spanned(
            field,
            "`choices` is only valid on `String` fields (or `Option<String>`) — \
             integer-valued enumerations should be modeled with a Rust enum and \
             custom (de)serializer for now",
        ));
    }
    if (attrs.min.is_some() || attrs.max.is_some()) && !kind.is_integer() {
        return Err(syn::Error::new_spanned(
            field,
            "`min` / `max` are only valid on integer fields (`i32`, `i64`, optionally Option-wrapped)",
        ));
    }
    if let (Some(min), Some(max)) = (attrs.min, attrs.max) {
        if min > max {
            return Err(syn::Error::new_spanned(
                field,
                format!("`min` ({min}) is greater than `max` ({max})"),
            ));
        }
    }
    Ok(())
}

fn optional_u32(value: Option<u32>) -> TokenStream2 {
    if let Some(v) = value {
        quote!(::core::option::Option::Some(#v))
    } else {
        quote!(::core::option::Option::None)
    }
}

fn optional_i64(value: Option<i64>) -> TokenStream2 {
    if let Some(v) = value {
        quote!(::core::option::Option::Some(#v))
    } else {
        quote!(::core::option::Option::None)
    }
}

fn optional_str(value: Option<&str>) -> TokenStream2 {
    if let Some(v) = value {
        quote!(::core::option::Option::Some(#v))
    } else {
        quote!(::core::option::Option::None)
    }
}

fn optional_choices(pairs: Option<&[(String, String)]>) -> TokenStream2 {
    let Some(pairs) = pairs else {
        return quote!(::core::option::Option::None);
    };
    let entries = pairs.iter().map(|(v, l)| quote!((#v, #l)));
    quote!(::core::option::Option::Some(&[#(#entries),*]))
}

fn relation_tokens(
    field: &syn::Field,
    attrs: &FieldAttrs,
    fk_inner: Option<&syn::Type>,
    table: &str,
) -> syn::Result<TokenStream2> {
    if let Some(inner) = fk_inner {
        if attrs.fk.is_some() || attrs.o2o.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "`ForeignKey<T>` already declares the FK target via the type parameter — \
                 remove the `fk = \"…\"` / `o2o = \"…\"` attribute.",
            ));
        }
        let on = attrs.on.as_deref().unwrap_or("id");
        return Ok(quote! {
            ::core::option::Option::Some(::rustango::core::Relation::Fk {
                to: <#inner as ::rustango::core::Model>::SCHEMA.table,
                on: #on,
            })
        });
    }
    match (&attrs.fk, &attrs.o2o) {
        (Some(_), Some(_)) => Err(syn::Error::new_spanned(
            field,
            "`fk` and `o2o` are mutually exclusive",
        )),
        (Some(to), None) => {
            let on = attrs.on.as_deref().unwrap_or("id");
            // Self-FK sentinel — `#[rustango(fk = "self")]` resolves to
            // the model's own table. Threaded as a literal string at
            // macro-expansion time to sidestep the const-eval cycle
            // that `Self::SCHEMA.table` would create when referenced
            // inside Self::SCHEMA's own initializer.
            let resolved = if to == "self" { table } else { to };
            Ok(quote! {
                ::core::option::Option::Some(::rustango::core::Relation::Fk { to: #resolved, on: #on })
            })
        }
        (None, Some(to)) => {
            let on = attrs.on.as_deref().unwrap_or("id");
            let resolved = if to == "self" { table } else { to };
            Ok(quote! {
                ::core::option::Option::Some(::rustango::core::Relation::O2O { to: #resolved, on: #on })
            })
        }
        (None, None) => {
            if attrs.on.is_some() {
                return Err(syn::Error::new_spanned(
                    field,
                    "`on` requires `fk` or `o2o`",
                ));
            }
            Ok(quote!(::core::option::Option::None))
        }
    }
}

/// Mirrors `rustango_core::FieldType`. Local copy so the macro can reason
/// about kinds without depending on `rustango-core` (which would require a
/// proc-macro/normal split it doesn't have today).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DetectedKind {
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
    String,
    DateTime,
    Date,
    Uuid,
    Json,
}

impl DetectedKind {
    fn variant_tokens(self) -> TokenStream2 {
        match self {
            Self::I16 => quote!(::rustango::core::FieldType::I16),
            Self::I32 => quote!(::rustango::core::FieldType::I32),
            Self::I64 => quote!(::rustango::core::FieldType::I64),
            Self::F32 => quote!(::rustango::core::FieldType::F32),
            Self::F64 => quote!(::rustango::core::FieldType::F64),
            Self::Bool => quote!(::rustango::core::FieldType::Bool),
            Self::String => quote!(::rustango::core::FieldType::String),
            Self::DateTime => quote!(::rustango::core::FieldType::DateTime),
            Self::Date => quote!(::rustango::core::FieldType::Date),
            Self::Uuid => quote!(::rustango::core::FieldType::Uuid),
            Self::Json => quote!(::rustango::core::FieldType::Json),
        }
    }

    fn is_integer(self) -> bool {
        matches!(self, Self::I16 | Self::I32 | Self::I64)
    }

    /// `(SqlValue::<Variant>, default expr)` for emitting the
    /// `match SqlValue { … }` arm in `LoadRelated::__rustango_load_related`
    /// for a `ForeignKey<T, K>` FK whose K maps to `self`. The default
    /// fires only when the parent's `__rustango_pk_value` returns a
    /// different variant than expected, which is a compile-time bug —
    /// but we still need a value-typed fallback to keep the match
    /// total.
    fn sqlvalue_match_arm(self) -> (TokenStream2, TokenStream2) {
        match self {
            Self::I16 => (quote!(I16), quote!(0i16)),
            Self::I32 => (quote!(I32), quote!(0i32)),
            Self::I64 => (quote!(I64), quote!(0i64)),
            Self::F32 => (quote!(F32), quote!(0f32)),
            Self::F64 => (quote!(F64), quote!(0f64)),
            Self::Bool => (quote!(Bool), quote!(false)),
            Self::String => (quote!(String), quote!(::std::string::String::new())),
            Self::DateTime => (
                quote!(DateTime),
                quote!(<::chrono::DateTime<::chrono::Utc> as ::std::default::Default>::default()),
            ),
            Self::Date => (
                quote!(Date),
                quote!(<::chrono::NaiveDate as ::std::default::Default>::default()),
            ),
            Self::Uuid => (quote!(Uuid), quote!(::uuid::Uuid::nil())),
            Self::Json => (quote!(Json), quote!(::serde_json::Value::Null)),
        }
    }
}

/// Result of walking a field's Rust type. `kind` is the underlying
/// `FieldType`; `nullable` is set by an outer `Option<T>`; `auto` is
/// set by an outer `Auto<T>` (server-assigned PK); `fk_inner` is
/// `Some(<T>)` when the field was `ForeignKey<T>` (or
/// `Option<ForeignKey<T>>`), letting the codegen reach `T::SCHEMA`.
#[derive(Clone, Copy)]
struct DetectedType<'a> {
    kind: DetectedKind,
    nullable: bool,
    auto: bool,
    fk_inner: Option<&'a syn::Type>,
}

/// Extract the `T` from a `…::Auto<T>` field type. Returns `None` for
/// non-`Auto` types — the caller should already have routed Auto-only
/// codegen through this helper, so a `None` indicates a macro-internal
/// invariant break.
fn auto_inner_type(ty: &syn::Type) -> Option<&syn::Type> {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return None;
    };
    let last = path.segments.last()?;
    if last.ident != "Auto" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

fn detect_type(ty: &syn::Type) -> syn::Result<DetectedType<'_>> {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return Err(syn::Error::new_spanned(ty, "unsupported field type"));
    };
    let last = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "empty type path"))?;

    if last.ident == "Option" {
        let inner = generic_inner(ty, &last.arguments, "Option")?;
        let inner_det = detect_type(inner)?;
        if inner_det.nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "nested Option is not supported",
            ));
        }
        if inner_det.auto {
            return Err(syn::Error::new_spanned(
                ty,
                "`Option<Auto<T>>` is not supported — Auto fields are server-assigned and cannot be NULL",
            ));
        }
        return Ok(DetectedType {
            nullable: true,
            ..inner_det
        });
    }

    if last.ident == "Auto" {
        let inner = generic_inner(ty, &last.arguments, "Auto")?;
        let inner_det = detect_type(inner)?;
        if inner_det.auto {
            return Err(syn::Error::new_spanned(ty, "nested Auto is not supported"));
        }
        if inner_det.nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "`Auto<Option<T>>` is not supported — Auto fields are server-assigned and cannot be NULL",
            ));
        }
        if inner_det.fk_inner.is_some() {
            return Err(syn::Error::new_spanned(
                ty,
                "`Auto<ForeignKey<T>>` is not supported — Auto is for server-assigned PKs, ForeignKey is for parent references",
            ));
        }
        if !matches!(
            inner_det.kind,
            DetectedKind::I32 | DetectedKind::I64 | DetectedKind::Uuid | DetectedKind::DateTime
        ) {
            return Err(syn::Error::new_spanned(
                ty,
                "`Auto<T>` only supports integers (`i32` → SERIAL, `i64` → BIGSERIAL), \
                 `uuid::Uuid` (DEFAULT gen_random_uuid()), or `chrono::DateTime<chrono::Utc>` \
                 (DEFAULT now())",
            ));
        }
        return Ok(DetectedType {
            auto: true,
            ..inner_det
        });
    }

    if last.ident == "ForeignKey" {
        let (inner, key_ty) = generic_pair(ty, &last.arguments, "ForeignKey")?;
        // Resolve the FK column's underlying SQL type from `K`. When the
        // user wrote `ForeignKey<T>` without a key parameter, the type
        // alias defaults to `i64` and we keep the v0.7 BIGINT shape.
        // When the user wrote `ForeignKey<T, K>` with an explicit `K`,
        // recurse into K so the column DDL emits the right SQL type
        // (VARCHAR for String, UUID for Uuid, …) and the load_related
        // emitter knows which `SqlValue` variant to match.
        let kind = match key_ty {
            Some(k) => detect_type(k)?.kind,
            None => DetectedKind::I64,
        };
        return Ok(DetectedType {
            kind,
            nullable: false,
            auto: false,
            fk_inner: Some(inner),
        });
    }

    let kind = match last.ident.to_string().as_str() {
        "i16" => DetectedKind::I16,
        "i32" => DetectedKind::I32,
        "i64" => DetectedKind::I64,
        "f32" => DetectedKind::F32,
        "f64" => DetectedKind::F64,
        "bool" => DetectedKind::Bool,
        "String" => DetectedKind::String,
        "DateTime" => DetectedKind::DateTime,
        "NaiveDate" => DetectedKind::Date,
        "Uuid" => DetectedKind::Uuid,
        "Value" => DetectedKind::Json,
        other => {
            return Err(syn::Error::new_spanned(
                ty,
                format!("unsupported field type `{other}`; v0.1 supports i32/i64/f32/f64/bool/String/DateTime/NaiveDate/Uuid/serde_json::Value, optionally wrapped in Option or Auto (Auto only on integers)"),
            ));
        }
    };
    Ok(DetectedType {
        kind,
        nullable: false,
        auto: false,
        fk_inner: None,
    })
}

fn generic_inner<'a>(
    ty: &'a Type,
    arguments: &'a PathArguments,
    wrapper: &str,
) -> syn::Result<&'a Type> {
    let PathArguments::AngleBracketed(args) = arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("{wrapper} requires a generic argument"),
        ));
    };
    args.args
        .iter()
        .find_map(|a| match a {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .ok_or_else(|| {
            syn::Error::new_spanned(ty, format!("{wrapper}<T> requires a type argument"))
        })
}

/// Like [`generic_inner`] but pulls *two* type args — the first is
/// required, the second is optional. Used by the `ForeignKey<T, K>`
/// detection where K defaults to `i64` when omitted.
fn generic_pair<'a>(
    ty: &'a Type,
    arguments: &'a PathArguments,
    wrapper: &str,
) -> syn::Result<(&'a Type, Option<&'a Type>)> {
    let PathArguments::AngleBracketed(args) = arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("{wrapper} requires a generic argument"),
        ));
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let first = types.next().ok_or_else(|| {
        syn::Error::new_spanned(ty, format!("{wrapper}<T> requires a type argument"))
    })?;
    let second = types.next();
    Ok((first, second))
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

// ============================================================
//  #[derive(Form)]  —  slice 8.4B
// ============================================================

/// Per-field `#[form(...)]` attributes recognised by the derive.
#[derive(Default)]
struct FormFieldAttrs {
    min: Option<i64>,
    max: Option<i64>,
    min_length: Option<u32>,
    max_length: Option<u32>,
}

/// Detected shape of a form field's Rust type.
#[derive(Clone, Copy)]
enum FormFieldKind {
    String,
    I16,
    I32,
    I64,
    F32,
    F64,
    Bool,
}

impl FormFieldKind {
    fn parse_method(self) -> &'static str {
        match self {
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            // String + Bool don't go through `str::parse`; the codegen
            // handles them inline.
            Self::String | Self::Bool => "",
        }
    }
}

fn expand_form(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Form can only be derived on structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Form requires a struct with named fields",
        ));
    };

    let mut field_blocks: Vec<TokenStream2> = Vec::with_capacity(named.named.len());
    let mut field_idents: Vec<&syn::Ident> = Vec::with_capacity(named.named.len());

    for field in &named.named {
        let ident = field
            .ident
            .as_ref()
            .ok_or_else(|| syn::Error::new(field.span(), "tuple structs are not supported"))?;
        let attrs = parse_form_field_attrs(field)?;
        let (kind, nullable) = detect_form_field(&field.ty, field.span())?;

        let name_lit = ident.to_string();
        let parse_block = render_form_field_parse(ident, &name_lit, kind, nullable, &attrs);
        field_blocks.push(parse_block);
        field_idents.push(ident);
    }

    Ok(quote! {
        impl ::rustango::forms::Form for #struct_name {
            fn parse(
                data: &::std::collections::HashMap<::std::string::String, ::std::string::String>,
            ) -> ::core::result::Result<Self, ::rustango::forms::FormErrors> {
                let mut __errors = ::rustango::forms::FormErrors::default();
                #( #field_blocks )*
                if !__errors.is_empty() {
                    return ::core::result::Result::Err(__errors);
                }
                ::core::result::Result::Ok(Self {
                    #( #field_idents ),*
                })
            }
        }
    })
}

fn parse_form_field_attrs(field: &syn::Field) -> syn::Result<FormFieldAttrs> {
    let mut out = FormFieldAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("form") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("min") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                out.min = Some(lit.base10_parse::<i64>()?);
                return Ok(());
            }
            if meta.path.is_ident("max") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                out.max = Some(lit.base10_parse::<i64>()?);
                return Ok(());
            }
            if meta.path.is_ident("min_length") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                out.min_length = Some(lit.base10_parse::<u32>()?);
                return Ok(());
            }
            if meta.path.is_ident("max_length") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                out.max_length = Some(lit.base10_parse::<u32>()?);
                return Ok(());
            }
            Err(meta.error(
                "unknown form attribute (supported: `min`, `max`, `min_length`, `max_length`)",
            ))
        })?;
    }
    Ok(out)
}

fn detect_form_field(ty: &Type, span: proc_macro2::Span) -> syn::Result<(FormFieldKind, bool)> {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return Err(syn::Error::new(
            span,
            "Form field must be a simple typed path (e.g. `String`, `i32`, `Option<String>`)",
        ));
    };
    let last = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new(span, "empty type path"))?;

    if last.ident == "Option" {
        let inner = generic_inner(ty, &last.arguments, "Option")?;
        let (kind, nested) = detect_form_field(inner, span)?;
        if nested {
            return Err(syn::Error::new(
                span,
                "nested Option in Form fields is not supported",
            ));
        }
        return Ok((kind, true));
    }

    let kind = match last.ident.to_string().as_str() {
        "String" => FormFieldKind::String,
        "i16" => FormFieldKind::I16,
        "i32" => FormFieldKind::I32,
        "i64" => FormFieldKind::I64,
        "f32" => FormFieldKind::F32,
        "f64" => FormFieldKind::F64,
        "bool" => FormFieldKind::Bool,
        other => {
            return Err(syn::Error::new(
                span,
                format!(
                    "Form field type `{other}` is not supported in v0.8 — use String / \
                     i16 / i32 / i64 / f32 / f64 / bool, optionally wrapped in Option<…>"
                ),
            ));
        }
    };
    Ok((kind, false))
}

#[allow(clippy::too_many_lines)]
fn render_form_field_parse(
    ident: &syn::Ident,
    name_lit: &str,
    kind: FormFieldKind,
    nullable: bool,
    attrs: &FormFieldAttrs,
) -> TokenStream2 {
    // Pull the raw &str from the payload. Uses variable name `data` to
    // match the new `Form::parse(data: &HashMap<…>)` signature.
    let lookup = quote! {
        let __raw: ::core::option::Option<&::std::string::String> = data.get(#name_lit);
    };

    let parsed_value = match kind {
        FormFieldKind::Bool => quote! {
            let __v: bool = match __raw {
                ::core::option::Option::None => false,
                ::core::option::Option::Some(__s) => !matches!(
                    __s.to_ascii_lowercase().as_str(),
                    "" | "false" | "0" | "off" | "no"
                ),
            };
        },
        FormFieldKind::String => {
            if nullable {
                quote! {
                    let __v: ::core::option::Option<::std::string::String> = match __raw {
                        ::core::option::Option::None => ::core::option::Option::None,
                        ::core::option::Option::Some(__s) if __s.is_empty() => {
                            ::core::option::Option::None
                        }
                        ::core::option::Option::Some(__s) => {
                            ::core::option::Option::Some(::core::clone::Clone::clone(__s))
                        }
                    };
                }
            } else {
                quote! {
                    let __v: ::std::string::String = match __raw {
                        ::core::option::Option::Some(__s) if !__s.is_empty() => {
                            ::core::clone::Clone::clone(__s)
                        }
                        _ => {
                            __errors.add(#name_lit, "This field is required.");
                            ::std::string::String::new()
                        }
                    };
                }
            }
        }
        FormFieldKind::I16
        | FormFieldKind::I32
        | FormFieldKind::I64
        | FormFieldKind::F32
        | FormFieldKind::F64 => {
            let parse_ty = syn::Ident::new(kind.parse_method(), proc_macro2::Span::call_site());
            let ty_lit = kind.parse_method();
            let default_val = match kind {
                FormFieldKind::I16 => quote! { 0i16 },
                FormFieldKind::I32 => quote! { 0i32 },
                FormFieldKind::I64 => quote! { 0i64 },
                FormFieldKind::F32 => quote! { 0f32 },
                FormFieldKind::F64 => quote! { 0f64 },
                _ => quote! { Default::default() },
            };
            if nullable {
                quote! {
                    let __v: ::core::option::Option<#parse_ty> = match __raw {
                        ::core::option::Option::None => ::core::option::Option::None,
                        ::core::option::Option::Some(__s) if __s.is_empty() => {
                            ::core::option::Option::None
                        }
                        ::core::option::Option::Some(__s) => {
                            match __s.parse::<#parse_ty>() {
                                ::core::result::Result::Ok(__n) => {
                                    ::core::option::Option::Some(__n)
                                }
                                ::core::result::Result::Err(__e) => {
                                    __errors.add(
                                        #name_lit,
                                        ::std::format!("Enter a valid {} value: {}", #ty_lit, __e),
                                    );
                                    ::core::option::Option::None
                                }
                            }
                        }
                    };
                }
            } else {
                quote! {
                    let __v: #parse_ty = match __raw {
                        ::core::option::Option::Some(__s) if !__s.is_empty() => {
                            match __s.parse::<#parse_ty>() {
                                ::core::result::Result::Ok(__n) => __n,
                                ::core::result::Result::Err(__e) => {
                                    __errors.add(
                                        #name_lit,
                                        ::std::format!("Enter a valid {} value: {}", #ty_lit, __e),
                                    );
                                    #default_val
                                }
                            }
                        }
                        _ => {
                            __errors.add(#name_lit, "This field is required.");
                            #default_val
                        }
                    };
                }
            }
        }
    };

    let validators = render_form_validators(name_lit, kind, nullable, attrs);

    quote! {
        let #ident = {
            #lookup
            #parsed_value
            #validators
            __v
        };
    }
}

fn render_form_validators(
    name_lit: &str,
    kind: FormFieldKind,
    nullable: bool,
    attrs: &FormFieldAttrs,
) -> TokenStream2 {
    let mut checks: Vec<TokenStream2> = Vec::new();

    let val_ref = if nullable {
        quote! { __v.as_ref() }
    } else {
        quote! { ::core::option::Option::Some(&__v) }
    };

    let is_string = matches!(kind, FormFieldKind::String);
    let is_numeric = matches!(
        kind,
        FormFieldKind::I16
            | FormFieldKind::I32
            | FormFieldKind::I64
            | FormFieldKind::F32
            | FormFieldKind::F64
    );

    if is_string {
        if let Some(min_len) = attrs.min_length {
            let min_len_usize = min_len as usize;
            checks.push(quote! {
                if let ::core::option::Option::Some(__s) = #val_ref {
                    if __s.len() < #min_len_usize {
                        __errors.add(
                            #name_lit,
                            ::std::format!("Ensure this value has at least {} characters.", #min_len_usize),
                        );
                    }
                }
            });
        }
        if let Some(max_len) = attrs.max_length {
            let max_len_usize = max_len as usize;
            checks.push(quote! {
                if let ::core::option::Option::Some(__s) = #val_ref {
                    if __s.len() > #max_len_usize {
                        __errors.add(
                            #name_lit,
                            ::std::format!("Ensure this value has at most {} characters.", #max_len_usize),
                        );
                    }
                }
            });
        }
    }

    if is_numeric {
        if let Some(min) = attrs.min {
            checks.push(quote! {
                if let ::core::option::Option::Some(__n) = #val_ref {
                    if (*__n as f64) < (#min as f64) {
                        __errors.add(
                            #name_lit,
                            ::std::format!("Ensure this value is greater than or equal to {}.", #min),
                        );
                    }
                }
            });
        }
        if let Some(max) = attrs.max {
            checks.push(quote! {
                if let ::core::option::Option::Some(__n) = #val_ref {
                    if (*__n as f64) > (#max as f64) {
                        __errors.add(
                            #name_lit,
                            ::std::format!("Ensure this value is less than or equal to {}.", #max),
                        );
                    }
                }
            });
        }
    }

    quote! { #( #checks )* }
}

// ============================================================
//  #[derive(ViewSet)]
// ============================================================

struct ViewSetAttrs {
    model: syn::Path,
    fields: Option<Vec<String>>,
    filter_fields: Vec<String>,
    search_fields: Vec<String>,
    /// (field_name, desc)
    ordering: Vec<(String, bool)>,
    page_size: Option<usize>,
    read_only: bool,
    perms: ViewSetPermsAttrs,
}

#[derive(Default)]
struct ViewSetPermsAttrs {
    list: Vec<String>,
    retrieve: Vec<String>,
    create: Vec<String>,
    update: Vec<String>,
    destroy: Vec<String>,
}

fn expand_viewset(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    // Must be a unit struct or an empty named struct.
    match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Unit | Fields::Named(_) => {}
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    struct_name,
                    "ViewSet can only be derived on a unit struct or an empty named struct",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                struct_name,
                "ViewSet can only be derived on a struct",
            ));
        }
    }

    let attrs = parse_viewset_attrs(input)?;
    let model_path = &attrs.model;

    // `.fields(&[...])` call — None means skip (use all scalar fields).
    let fields_call = if let Some(ref fields) = attrs.fields {
        let lits = fields.iter().map(|f| f.as_str());
        quote!(.fields(&[ #(#lits),* ]))
    } else {
        quote!()
    };

    let filter_fields_call = if attrs.filter_fields.is_empty() {
        quote!()
    } else {
        let lits = attrs.filter_fields.iter().map(|f| f.as_str());
        quote!(.filter_fields(&[ #(#lits),* ]))
    };

    let search_fields_call = if attrs.search_fields.is_empty() {
        quote!()
    } else {
        let lits = attrs.search_fields.iter().map(|f| f.as_str());
        quote!(.search_fields(&[ #(#lits),* ]))
    };

    let ordering_call = if attrs.ordering.is_empty() {
        quote!()
    } else {
        let pairs = attrs.ordering.iter().map(|(f, desc)| {
            let f = f.as_str();
            quote!((#f, #desc))
        });
        quote!(.ordering(&[ #(#pairs),* ]))
    };

    let page_size_call = if let Some(n) = attrs.page_size {
        quote!(.page_size(#n))
    } else {
        quote!()
    };

    let read_only_call = if attrs.read_only {
        quote!(.read_only())
    } else {
        quote!()
    };

    let perms = &attrs.perms;
    let perms_call = if perms.list.is_empty()
        && perms.retrieve.is_empty()
        && perms.create.is_empty()
        && perms.update.is_empty()
        && perms.destroy.is_empty()
    {
        quote!()
    } else {
        let list_lits = perms.list.iter().map(|s| s.as_str());
        let retrieve_lits = perms.retrieve.iter().map(|s| s.as_str());
        let create_lits = perms.create.iter().map(|s| s.as_str());
        let update_lits = perms.update.iter().map(|s| s.as_str());
        let destroy_lits = perms.destroy.iter().map(|s| s.as_str());
        quote! {
            .permissions(::rustango::viewset::ViewSetPerms {
                list:     ::std::vec![ #(#list_lits.to_owned()),* ],
                retrieve: ::std::vec![ #(#retrieve_lits.to_owned()),* ],
                create:   ::std::vec![ #(#create_lits.to_owned()),* ],
                update:   ::std::vec![ #(#update_lits.to_owned()),* ],
                destroy:  ::std::vec![ #(#destroy_lits.to_owned()),* ],
            })
        }
    };

    Ok(quote! {
        impl #struct_name {
            /// Build an `axum::Router` with the six standard REST endpoints
            /// for this ViewSet, mounted at `prefix`.
            pub fn router(prefix: &str, pool: ::rustango::sql::sqlx::PgPool) -> ::axum::Router {
                ::rustango::viewset::ViewSet::for_model(
                    <#model_path as ::rustango::core::Model>::SCHEMA
                )
                    #fields_call
                    #filter_fields_call
                    #search_fields_call
                    #ordering_call
                    #page_size_call
                    #perms_call
                    #read_only_call
                    .router(prefix, pool)
            }
        }
    })
}

fn parse_viewset_attrs(input: &DeriveInput) -> syn::Result<ViewSetAttrs> {
    let mut model: Option<syn::Path> = None;
    let mut fields: Option<Vec<String>> = None;
    let mut filter_fields: Vec<String> = Vec::new();
    let mut search_fields: Vec<String> = Vec::new();
    let mut ordering: Vec<(String, bool)> = Vec::new();
    let mut page_size: Option<usize> = None;
    let mut read_only = false;
    let mut perms = ViewSetPermsAttrs::default();

    for attr in &input.attrs {
        if !attr.path().is_ident("viewset") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("model") {
                let path: syn::Path = meta.value()?.parse()?;
                model = Some(path);
                return Ok(());
            }
            if meta.path.is_ident("fields") {
                let s: LitStr = meta.value()?.parse()?;
                fields = Some(split_field_list(&s.value()));
                return Ok(());
            }
            if meta.path.is_ident("filter_fields") {
                let s: LitStr = meta.value()?.parse()?;
                filter_fields = split_field_list(&s.value());
                return Ok(());
            }
            if meta.path.is_ident("search_fields") {
                let s: LitStr = meta.value()?.parse()?;
                search_fields = split_field_list(&s.value());
                return Ok(());
            }
            if meta.path.is_ident("ordering") {
                let s: LitStr = meta.value()?.parse()?;
                ordering = parse_ordering_list(&s.value());
                return Ok(());
            }
            if meta.path.is_ident("page_size") {
                let lit: syn::LitInt = meta.value()?.parse()?;
                page_size = Some(lit.base10_parse::<usize>()?);
                return Ok(());
            }
            if meta.path.is_ident("read_only") {
                read_only = true;
                return Ok(());
            }
            if meta.path.is_ident("permissions") {
                meta.parse_nested_meta(|inner| {
                    let parse_codenames = |inner: &syn::meta::ParseNestedMeta| -> syn::Result<Vec<String>> {
                        let s: LitStr = inner.value()?.parse()?;
                        Ok(split_field_list(&s.value()))
                    };
                    if inner.path.is_ident("list") {
                        perms.list = parse_codenames(&inner)?;
                    } else if inner.path.is_ident("retrieve") {
                        perms.retrieve = parse_codenames(&inner)?;
                    } else if inner.path.is_ident("create") {
                        perms.create = parse_codenames(&inner)?;
                    } else if inner.path.is_ident("update") {
                        perms.update = parse_codenames(&inner)?;
                    } else if inner.path.is_ident("destroy") {
                        perms.destroy = parse_codenames(&inner)?;
                    } else {
                        return Err(inner.error(
                            "unknown permissions key (supported: list, retrieve, create, update, destroy)",
                        ));
                    }
                    Ok(())
                })?;
                return Ok(());
            }
            Err(meta.error(
                "unknown viewset attribute (supported: model, fields, filter_fields, \
                 search_fields, ordering, page_size, read_only, permissions(...))",
            ))
        })?;
    }

    let model = model.ok_or_else(|| {
        syn::Error::new_spanned(&input.ident, "`#[viewset(model = SomeModel)]` is required")
    })?;

    Ok(ViewSetAttrs {
        model,
        fields,
        filter_fields,
        search_fields,
        ordering,
        page_size,
        read_only,
        perms,
    })
}

// ============================================================ #[derive(Serializer)]

struct SerializerContainerAttrs {
    model: syn::Path,
}

#[derive(Default)]
struct SerializerFieldAttrs {
    read_only: bool,
    write_only: bool,
    source: Option<String>,
    skip: bool,
    /// `#[serializer(method = "fn_name")]` — DRF SerializerMethodField
    /// analog. The macro emits `from_model` initializer that calls
    /// `Self::fn_name(&model)` and stores the return value.
    method: Option<String>,
    /// `#[serializer(validate = "fn_name")]` — per-field validator
    /// callable run by `Self::validate(&self)`. Must return
    /// `Result<(), String>`. Errors land in `FormErrors` keyed by
    /// the field name.
    validate: Option<String>,
    /// `#[serializer(nested)]` on a field whose type is another
    /// `Serializer` — the macro emits `from_model` initializer that
    /// reads the parent via `model.<source>.value()` then calls the
    /// child serializer's `from_model(parent)`. When the FK is
    /// unloaded the field falls back to `Default::default()` (does
    /// NOT panic) so a missing prefetch in prod degrades gracefully.
    /// Source field on the model defaults to the field name; override
    /// with `source = "..."`. Combine with `strict` to keep the v0.18.1
    /// panic-on-unloaded behavior for tests.
    nested: bool,
    /// `#[serializer(nested, strict)]` — opt back into the v0.18.1
    /// strict behavior: panic when the FK isn't loaded. Useful in
    /// test code where forgetting select_related must trip a hard
    /// failure rather than render a blank nested object.
    nested_strict: bool,
    /// `#[serializer(many = TagSerializer)]` — declare the field as
    /// a list of nested serializers. Field type must be `Vec<S>`
    /// where `S` is the inner serializer. The macro initializes the
    /// field to `Vec::new()` in `from_model` and emits a typed
    /// `set_<field>(&mut self, models: &[<S::Model>])` helper that
    /// maps each model row through `S::from_model`. Auto-load isn't
    /// possible (the M2M / one-to-many accessor is async); callers
    /// fetch the children + call the setter post-from_model.
    many: Option<syn::Type>,
    /// `#[serializer(slug = "name")]` — DRF `SlugRelatedField` analog.
    /// Source field on the model must be a `ForeignKey<T>`; the
    /// macro emits `from_model` glue that walks
    /// `model.<source>.value()?.<slug>` and clones it. Field type on
    /// the serializer is typically `String` (whatever type the slug
    /// column has). When the FK is unloaded the field falls back to
    /// `Default::default()`, same graceful-degrade contract as
    /// `nested`. Source defaults to the field name; override with
    /// `source = "..."`. v0.44.
    slug: Option<String>,
}

fn parse_serializer_container_attrs(input: &DeriveInput) -> syn::Result<SerializerContainerAttrs> {
    let mut model: Option<syn::Path> = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("serializer") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("model") {
                let _eq: syn::Token![=] = meta.input.parse()?;
                model = Some(meta.input.parse()?);
                return Ok(());
            }
            Err(meta.error("unknown serializer container attribute (supported: `model`)"))
        })?;
    }
    let model = model.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "`#[serializer(model = SomeModel)]` is required",
        )
    })?;
    Ok(SerializerContainerAttrs { model })
}

fn parse_serializer_field_attrs(field: &syn::Field) -> syn::Result<SerializerFieldAttrs> {
    let mut out = SerializerFieldAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("serializer") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("read_only") {
                out.read_only = true;
                return Ok(());
            }
            if meta.path.is_ident("write_only") {
                out.write_only = true;
                return Ok(());
            }
            if meta.path.is_ident("skip") {
                out.skip = true;
                return Ok(());
            }
            if meta.path.is_ident("source") {
                let s: LitStr = meta.value()?.parse()?;
                out.source = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("method") {
                let s: LitStr = meta.value()?.parse()?;
                out.method = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("validate") {
                let s: LitStr = meta.value()?.parse()?;
                out.validate = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("many") {
                let _eq: syn::Token![=] = meta.input.parse()?;
                out.many = Some(meta.input.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("nested") {
                out.nested = true;
                // Optional strict flag inside parentheses:
                //   #[serializer(nested(strict))]
                if meta.input.peek(syn::token::Paren) {
                    meta.parse_nested_meta(|inner| {
                        if inner.path.is_ident("strict") {
                            out.nested_strict = true;
                            return Ok(());
                        }
                        Err(inner.error("unknown nested sub-attribute (supported: `strict`)"))
                    })?;
                }
                return Ok(());
            }
            if meta.path.is_ident("slug") {
                let s: LitStr = meta.value()?.parse()?;
                out.slug = Some(s.value());
                return Ok(());
            }
            Err(meta.error(
                "unknown serializer field attribute (supported: \
                 `read_only`, `write_only`, `source`, `skip`, `method`, \
                 `validate`, `nested`, `many`, `slug`)",
            ))
        })?;
    }
    // Validate: read_only + write_only is nonsensical
    if out.read_only && out.write_only {
        return Err(syn::Error::new_spanned(
            field,
            "a field cannot be both `read_only` and `write_only`",
        ));
    }
    if out.method.is_some() && out.source.is_some() {
        return Err(syn::Error::new_spanned(
            field,
            "`method` and `source` are mutually exclusive — `method` computes \
             the value from a method, `source` reads it from a different model field",
        ));
    }
    if out.slug.is_some() && (out.method.is_some() || out.nested || out.many.is_some()) {
        return Err(syn::Error::new_spanned(
            field,
            "`slug` is mutually exclusive with `method`, `nested`, and `many` \
             — pick one strategy for populating the field",
        ));
    }
    Ok(out)
}

fn expand_serializer(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;
    let struct_name_lit = struct_name.to_string();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Serializer can only be derived on structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "Serializer requires a struct with named fields",
        ));
    };

    let container = parse_serializer_container_attrs(input)?;
    let model_path = &container.model;

    // Classify each field. `ty` is only consumed by the
    // `#[cfg(feature = "openapi")]` block below, but we always
    // capture it to keep the field-info build a single pass.
    #[allow(dead_code)]
    struct FieldInfo {
        ident: syn::Ident,
        ty: syn::Type,
        attrs: SerializerFieldAttrs,
    }
    let mut fields_info: Vec<FieldInfo> = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().expect("named field has ident");
        let attrs = parse_serializer_field_attrs(field)?;
        fields_info.push(FieldInfo {
            ident,
            ty: field.ty.clone(),
            attrs,
        });
    }

    // Generate from_model body: struct literal with each field assigned.
    let from_model_fields = fields_info.iter().map(|fi| {
        let ident = &fi.ident;
        let ty = &fi.ty;
        if let Some(_inner) = &fi.attrs.many {
            // Many — collection field. Initialize empty; caller
            // populates via the macro-emitted set_<field> helper
            // after fetching the M2M children.
            quote! { #ident: ::std::vec::Vec::new() }
        } else if let Some(method) = &fi.attrs.method {
            // SerializerMethodField: call Self::<method>(&model) to
            // compute the value. Method signature must be
            // `fn <method>(model: &T) -> <field type>`.
            let method_ident = syn::Ident::new(method, ident.span());
            quote! { #ident: Self::#method_ident(model) }
        } else if let Some(slug_field) = &fi.attrs.slug {
            // v0.44 — SlugRelatedField. Source defaults to the field
            // name on this struct; override via `source = "..."`. The
            // source field on the model is expected to be a
            // `ForeignKey<T>`; the slug field on the parent is named
            // by the attribute value. When the FK is unloaded the
            // field falls back to `Default::default()` — same
            // graceful-degrade contract as `nested`.
            let src_name = fi
                .attrs
                .source
                .as_deref()
                .unwrap_or(&fi.ident.to_string())
                .to_owned();
            let src_ident = syn::Ident::new(&src_name, ident.span());
            let slug_ident = syn::Ident::new(slug_field, ident.span());
            quote! {
                #ident: match model.#src_ident.value() {
                    ::core::option::Option::Some(__loaded) =>
                        ::core::clone::Clone::clone(&__loaded.#slug_ident),
                    ::core::option::Option::None =>
                        ::core::default::Default::default(),
                }
            }
        } else if fi.attrs.nested {
            // Nested serializer. Source defaults to the field name on
            // this struct; override via `source = "..."`. The source
            // field on the model is expected to be a `ForeignKey<T>`
            // whose `.value()` returns `Option<&T>` after lazy-load.
            //
            // Behavior matrix (tweakable per-field):
            //   * FK loaded   → nested object materializes via
            //                   ChildSerializer::from_model(parent).
            //   * FK unloaded → fall back to ChildSerializer::default()
            //                   (so prod doesn't crash on a missing
            //                   prefetch — just renders a blank nested
            //                   object). Add `#[serializer(nested,
            //                   strict)]` to keep the v0.18.1
            //                   panic-on-unloaded behavior for tests
            //                   that want hard guardrails.
            let src_name = fi.attrs.source.as_deref().unwrap_or(&fi.ident.to_string()).to_owned();
            let src_ident = syn::Ident::new(&src_name, ident.span());
            if fi.attrs.nested_strict {
                let panic_msg = format!(
                    "nested(strict) serializer for `{ident}` requires `model.{src_name}` to be loaded — \
                     call .get(&pool).await? or .select_related(\"{src_name}\") on the model first",
                );
                quote! {
                    #ident: <#ty as ::rustango::serializer::ModelSerializer>::from_model(
                        model.#src_ident.value().expect(#panic_msg),
                    )
                }
            } else {
                quote! {
                    #ident: match model.#src_ident.value() {
                        ::core::option::Option::Some(__loaded) =>
                            <#ty as ::rustango::serializer::ModelSerializer>::from_model(__loaded),
                        ::core::option::Option::None =>
                            ::core::default::Default::default(),
                    }
                }
            }
        } else if fi.attrs.write_only || fi.attrs.skip {
            // Not read from model — use default
            quote! { #ident: ::core::default::Default::default() }
        } else if let Some(src) = &fi.attrs.source {
            let src_ident = syn::Ident::new(src, ident.span());
            quote! { #ident: ::core::clone::Clone::clone(&model.#src_ident) }
        } else {
            quote! { #ident: ::core::clone::Clone::clone(&model.#ident) }
        }
    });

    // Per-field validators (DRF-shape `validators=[...]`). Emit a
    // `validate(&self)` method that runs each user-defined validator
    // and aggregates errors into `FormErrors`.
    let validator_calls: Vec<_> = fields_info
        .iter()
        .filter_map(|fi| {
            let ident = &fi.ident;
            let name_lit = ident.to_string();
            let method = fi.attrs.validate.as_ref()?;
            let method_ident = syn::Ident::new(method, ident.span());
            Some(quote! {
                if let ::core::result::Result::Err(__e) = Self::#method_ident(&self.#ident) {
                    __errors.add(#name_lit.to_owned(), __e);
                }
            })
        })
        .collect();
    let validate_method = if validator_calls.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #struct_name {
                /// Run every `#[serializer(validate = "...")]` per-field
                /// validator. Aggregates errors into `FormErrors` keyed
                /// by the field name. Returns `Ok(())` when all pass.
                pub fn validate(&self) -> ::core::result::Result<(), ::rustango::forms::FormErrors> {
                    let mut __errors = ::rustango::forms::FormErrors::default();
                    #( #validator_calls )*
                    if __errors.is_empty() {
                        ::core::result::Result::Ok(())
                    } else {
                        ::core::result::Result::Err(__errors)
                    }
                }
            }
        }
    };

    // For every `#[serializer(many = S)]` field, emit a
    // `pub fn set_<field>(&mut self, models: &[<S::Model>]) -> &mut Self`
    // helper that maps the parents through `S::from_model`.
    let many_setters: Vec<_> = fields_info
        .iter()
        .filter_map(|fi| {
            let many_ty = fi.attrs.many.as_ref()?;
            let ident = &fi.ident;
            let setter = syn::Ident::new(&format!("set_{ident}"), ident.span());
            Some(quote! {
                /// Populate this `many` field by mapping each parent model
                /// through the inner serializer's `from_model`. Call after
                /// fetching the M2M / one-to-many children since
                /// `from_model` itself can't await an SQL query.
                pub fn #setter(
                    &mut self,
                    models: &[<#many_ty as ::rustango::serializer::ModelSerializer>::Model],
                ) -> &mut Self {
                    self.#ident = models.iter()
                        .map(<#many_ty as ::rustango::serializer::ModelSerializer>::from_model)
                        .collect();
                    self
                }
            })
        })
        .collect();
    let many_setters_impl = if many_setters.is_empty() {
        quote! {}
    } else {
        quote! {
            impl #struct_name {
                #( #many_setters )*
            }
        }
    };

    // Generate custom Serialize: skip write_only fields
    let output_fields: Vec<_> = fields_info
        .iter()
        .filter(|fi| !fi.attrs.write_only)
        .collect();
    let output_field_count = output_fields.len();
    let serialize_fields = output_fields.iter().map(|fi| {
        let ident = &fi.ident;
        let name_lit = ident.to_string();
        quote! { __state.serialize_field(#name_lit, &self.#ident)?; }
    });

    // writable_fields: normal + write_only.
    // Exclude:
    //   - `read_only` — server-computed.
    //   - `skip` — caller sets manually post-from_model.
    //   - `method` — computed from a Self::fn(&model) call; accepting
    //     it on write is meaningless.
    //   - `nested` / `many` — populated from related-model data, not
    //     from a field on the wire body.
    // v0.44 fix: pre-v0.44 the macro included `method` / `nested` /
    // `many` in `writable_fields()`, which made the ViewSet write
    // path accept those fields from the JSON body and try to bind
    // them to the SQL UPDATE — a silent no-op at best, a type
    // mismatch at worst.
    let writable_lits: Vec<_> = fields_info
        .iter()
        .filter(|fi| {
            !fi.attrs.read_only
                && !fi.attrs.skip
                && fi.attrs.method.is_none()
                && !fi.attrs.nested
                && fi.attrs.many.is_none()
                && fi.attrs.slug.is_none()
        })
        .map(|fi| fi.ident.to_string())
        .collect();

    // OpenAPI: emit `impl OpenApiSchema` when our `openapi` feature is on.
    // Only includes fields shown in JSON output (skips write_only). For each
    // `Option<T>` field, omit from `required` and add `.nullable()`.
    let openapi_impl = {
        #[cfg(feature = "openapi")]
        {
            let property_calls = output_fields.iter().map(|fi| {
                let ident = &fi.ident;
                let name_lit = ident.to_string();
                let ty = &fi.ty;
                let nullable_call = if is_option(ty) {
                    quote! { .nullable() }
                } else {
                    quote! {}
                };
                quote! {
                    .property(
                        #name_lit,
                        <#ty as ::rustango::openapi::OpenApiSchema>::openapi_schema()
                            #nullable_call,
                    )
                }
            });
            let required_lits: Vec<_> = output_fields
                .iter()
                .filter(|fi| !is_option(&fi.ty))
                .map(|fi| fi.ident.to_string())
                .collect();
            quote! {
                impl ::rustango::openapi::OpenApiSchema for #struct_name {
                    fn openapi_schema() -> ::rustango::openapi::Schema {
                        ::rustango::openapi::Schema::object()
                            #( #property_calls )*
                            .required([ #( #required_lits ),* ])
                    }
                }
            }
        }
        #[cfg(not(feature = "openapi"))]
        {
            quote! {}
        }
    };

    Ok(quote! {
        impl ::rustango::serializer::ModelSerializer for #struct_name {
            type Model = #model_path;

            fn from_model(model: &Self::Model) -> Self {
                Self {
                    #( #from_model_fields ),*
                }
            }

            fn writable_fields() -> &'static [&'static str] {
                &[ #( #writable_lits ),* ]
            }
        }

        impl ::serde::Serialize for #struct_name {
            fn serialize<S>(&self, serializer: S)
                -> ::core::result::Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                use ::serde::ser::SerializeStruct;
                let mut __state = serializer.serialize_struct(
                    #struct_name_lit,
                    #output_field_count,
                )?;
                #( #serialize_fields )*
                __state.end()
            }
        }

        #openapi_impl

        #validate_method

        #many_setters_impl
    })
}

/// Returns true if `ty` looks like `Option<T>` (any path ending in `Option`).
/// Only used by the `openapi`-gated emission of `OpenApiSchema`; muted
/// when the feature is off.
#[cfg_attr(not(feature = "openapi"), allow(dead_code))]
fn is_option(ty: &syn::Type) -> bool {
    if let syn::Type::Path(p) = ty {
        if let Some(last) = p.path.segments.last() {
            return last.ident == "Option";
        }
    }
    false
}
