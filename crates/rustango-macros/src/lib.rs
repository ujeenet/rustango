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

/// Derive `rustango::forms::FormStruct` (slice 8.4B). Generates a
/// `parse(&HashMap<String, String>) -> Result<Self, FormError>` impl
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

/// Derive [`rustango::serializer::ModelSerializer`] for a struct.
///
/// # Container attribute (required)
/// `#[serializer(model = TypeName)]` — the [`Model`] type this serializer maps from.
///
/// # Field attributes
/// - `#[serializer(read_only)]` — mapped from model; included in JSON output; excluded from `writable_fields()`
/// - `#[serializer(write_only)]` — `Default::default()` in `from_model`; excluded from JSON output; included in `writable_fields()`
/// - `#[serializer(source = "field_name")]` — reads from `model.field_name` instead of `model.<field_ident>`
/// - `#[serializer(skip)]` — `Default::default()` in `from_model`; included in JSON output; excluded from `writable_fields()` (user sets manually)
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

fn expand_main(
    args: TokenStream2,
    item: TokenStream2,
) -> syn::Result<TokenStream2> {
    let mut input: syn::ItemFn = syn::parse2(item)?;
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new(
            input.sig.ident.span(),
            "`#[rustango::main]` must wrap an `async fn`",
        ));
    }

    // Parse optional `flavor = "..."` etc. from the attribute args
    // and pass them straight through to `#[tokio::main(...)]`.
    let tokio_attr = if args.is_empty() {
        quote! { #[::tokio::main] }
    } else {
        quote! { #[::tokio::main(#args)] }
    };

    // Re-block the body so the tracing init runs before user code.
    let body = input.block.clone();
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
        #body
    }})?;

    Ok(quote! {
        #tokio_attr
        #input
    })
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
        let prev = json
            .get("prev")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
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

    let collected = collect_fields(named)?;

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
    if let Some(admin) = &container.admin {
        for (label, list) in [
            ("list_display", &admin.list_display),
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
        &collected.field_schemas,
        collected.soft_delete_column.as_deref(),
        container.permissions,
        audit_track_names.as_deref(),
        &container.m2m,
        &all_indexes,
        &container.checks,
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
    );
    let column_module = column_module_tokens(&module_ident, struct_name, &collected.column_entries);
    let from_row_impl = from_row_impl_tokens(struct_name, &collected.from_row_inits);
    let reverse_helpers = reverse_helper_tokens(struct_name, &collected.fk_relations);
    let m2m_accessors = m2m_accessor_tokens(struct_name, &container.m2m);

    Ok(quote! {
        #model_impl
        #inherent_impl
        #from_row_impl
        #column_module
        #reverse_helpers
        #m2m_accessors

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
fn load_related_impl_tokens(
    struct_name: &syn::Ident,
    fk_relations: &[FkRelation],
) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let parent_ty = &rel.parent_type;
        let fk_col = rel.fk_column.as_str();
        // FK field's Rust ident matches its SQL column name in v0.8
        // (no `column = "..."` rename ships on FK fields).
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        quote! {
            #fk_col => {
                let _parent: #parent_ty = <#parent_ty>::__rustango_from_aliased_row(row, alias)?;
                let _pk = match <#parent_ty>::__rustango_pk_value(&_parent) {
                    ::rustango::core::SqlValue::I64(v) => v,
                    _ => 0i64,
                };
                self.#field_ident = ::rustango::sql::ForeignKey::loaded(_pk, _parent);
                ::core::result::Result::Ok(true)
            }
        }
    });
    quote! {
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

/// Emit `impl FkPkAccess for #StructName` — slice 9.0e. Pattern-
/// matches `field_name` against the model's FK fields and returns
/// the FK's stored PK as `i64`. Used by `fetch_with_prefetch` to
/// group children by parent PK.
///
/// Always emitted (with `_ => None` for FK-less models) so the
/// trait bound on `fetch_with_prefetch` is universally satisfied.
fn fk_pk_access_impl_tokens(
    struct_name: &syn::Ident,
    fk_relations: &[FkRelation],
) -> TokenStream2 {
    let arms = fk_relations.iter().map(|rel| {
        let fk_col = rel.fk_column.as_str();
        let field_ident = syn::Ident::new(fk_col, proc_macro2::Span::call_site());
        quote! {
            #fk_col => ::core::option::Option::Some(self.#field_ident.pk()),
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
        }
    }
}

/// For every `ForeignKey<Parent>` field on `Child`, emit
/// `impl Parent { pub async fn <child_table>_set(&self, executor) -> Vec<Child> }`.
/// Reads the parent's PK via the macro-generated `__rustango_pk_value`
/// and runs a single `SELECT … FROM <child_table> WHERE <fk_column> = $1`
/// — the canonical reverse-FK fetch. One round trip, no N+1.
fn reverse_helper_tokens(
    child_ident: &syn::Ident,
    fk_relations: &[FkRelation],
) -> TokenStream2 {
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
                        .filter(#fk_col, ::rustango::core::Op::Eq, _pk)
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
    /// Inner type of `ForeignKey<T>` — the parent model. The reverse
    /// helper is emitted as `impl <ParentType> { … }`.
    parent_type: Type,
    /// SQL column name on the child table for this FK (e.g. `"author"`).
    /// Used in the generated `WHERE <fk_column> = $1` clause.
    fk_column: String,
}

fn collect_fields(named: &syn::FieldsNamed) -> syn::Result<CollectedFields> {
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
        let info = process_field(field)?;
        out.field_names.push(info.ident.to_string());
        out.field_schemas.push(info.schema);
        out.from_row_inits.push(info.from_row_init);
        out.from_aliased_row_inits.push(info.from_aliased_row_init);
        if let Some(parent_ty) = info.fk_inner.clone() {
            out.fk_relations.push(FkRelation {
                parent_type: parent_ty,
                fk_column: info.column.clone(),
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
            }
            out.returning_cols.push(quote!(#column));
            out.auto_field_idents
                .push((ident.clone(), info.column.clone()));
            out.auto_assigns.push(quote! {
                self.#ident = ::rustango::sql::sqlx::Row::try_get(&_returning_row, #column)?;
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
                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                        ::chrono::Utc::now()
                    ),
                }
            });
            out.upsert_update_columns.push(quote!(#column));
        } else {
            out.update_assignments.push(quote! {
                ::rustango::core::Assignment {
                    column: #column,
                    value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                        ::core::clone::Clone::clone(&self.#ident)
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
    field_schemas: &[TokenStream2],
    soft_delete_column: Option<&str>,
    permissions: bool,
    audit_track: Option<&[String]>,
    m2m_relations: &[M2MAttr],
    indexes: &[IndexAttr],
    checks: &[CheckAttr],
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
    let indexes_tokens = indexes.iter().map(|idx| {
        let name = idx.name.as_deref().unwrap_or("unnamed_index");
        let cols: Vec<&str> = idx.columns.iter().map(String::as_str).collect();
        let unique = idx.unique;
        quote! {
            ::rustango::core::IndexSchema {
                name: #name,
                columns: &[ #(#cols),* ],
                unique: #unique,
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

    // Update emission captures both BEFORE and AFTER state — runs an
    // extra SELECT against `_executor` BEFORE the UPDATE, captures
    // each tracked field's prior value, then after the UPDATE diffs
    // against the in-memory `&self`. `diff_changes` drops unchanged
    // columns so the JSON only contains the actual delta.
    //
    // Two-fragment shape: `audit_update_pre` runs before the UPDATE
    // and binds `_audit_before_pairs`; `audit_update_post` runs
    // after the UPDATE and emits the PendingEntry.
    let (audit_update_pre, audit_update_post): (TokenStream2, TokenStream2) =
        if let Some(tracked) = audited_fields {
            if tracked.is_empty() {
                (quote!(), quote!())
            } else {
                let select_cols: String = tracked
                    .iter()
                    .map(|c| format!("\"{}\"", c.column.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(", ");
                let pk_column_for_select = primary_key
                    .map(|(_, col)| col.clone())
                    .unwrap_or_default();
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
        let row_pairs = audited_fields
            .unwrap_or(&[])
            .iter()
            .map(|c| {
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
        let (pk_ident, pk_column) = primary_key
            .expect("pk_is_auto implies primary_key is Some");
        let pk_column_lit = pk_column.as_str();
        let assignments = &fields.update_assignments;
        let upsert_cols = &fields.upsert_update_columns;
        let upsert_pushes = &fields.insert_pushes;
        let upsert_returning = &fields.returning_cols;
        let upsert_auto_assigns = &fields.auto_assigns;
        let conflict_clause = if fields.upsert_update_columns.is_empty() {
            quote!(::rustango::core::ConflictClause::DoNothing)
        } else {
            quote!(::rustango::core::ConflictClause::DoUpdate {
                target: ::std::vec![#pk_column_lit],
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
                let _ = ::rustango::sql::update_on(
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
            pub async fn upsert(
                &mut self,
                pool: &::rustango::sql::sqlx::PgPool,
            ) -> ::core::result::Result<(), ::rustango::sql::ExecError> {
                self.upsert_on(pool).await
            }

            /// Like [`Self::upsert`] but accepts any sqlx executor.
            /// See [`Self::save_on`] for tenancy-scoped rationale.
            ///
            /// # Errors
            /// As [`Self::upsert`].
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
                let _returning_row = ::rustango::sql::insert_returning_on(
                    #executor_passes_to_data_write,
                    &query,
                ).await?;
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
                                value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                    ::chrono::Utc::now()
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
                    let _affected = ::rustango::sql::update_on(
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
                                value: ::rustango::core::SqlValue::Null,
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
                    let _affected = ::rustango::sql::update_on(
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
                let _affected = ::rustango::sql::delete_on(
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
            pub async fn delete_on_with #executor_generics (
                &self,
                #executor_param,
                source: ::rustango::audit::AuditSource,
            ) -> ::core::result::Result<u64, ::rustango::sql::ExecError>
            #executor_where
            {
                ::rustango::audit::with_source(source, self.delete_on(_executor)).await
            }
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
                let _returning_row = ::rustango::sql::insert_returning_on(
                    #executor_passes_to_data_write,
                    &query,
                ).await?;
                #( #auto_assigns )*
                #audit_insert_emit
                ::core::result::Result::Ok(())
            }

            /// Per-call audit-source override for [`Self::insert_on`].
            /// See [`Self::save_on_with`] for shape rationale.
            ///
            /// # Errors
            /// As [`Self::insert_on`].
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
                ::rustango::sql::insert_on(_executor, &query).await
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
                let _returned = ::rustango::sql::bulk_insert_on(
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
                let _ = ::rustango::sql::bulk_insert_on(_executor, &_query).await?;
                ::core::result::Result::Ok(())
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

    let from_aliased_row_inits = &fields.from_aliased_row_inits;
    let aliased_row_helper = quote! {
        /// Decode a row's aliased target columns (produced by
        /// `select_related`'s LEFT JOIN) into a fresh instance of
        /// this model. Reads each column via
        /// `format!("{prefix}__{col}")`, matching the alias the
        /// SELECT writer emitted. Slice 9.0d.
        #[doc(hidden)]
        pub fn __rustango_from_aliased_row(
            row: &::rustango::sql::sqlx::postgres::PgRow,
            prefix: &str,
        ) -> ::core::result::Result<Self, ::rustango::sql::sqlx::Error> {
            ::core::result::Result::Ok(Self {
                #( #from_aliased_row_inits ),*
            })
        }
    };

    let load_related_impl =
        load_related_impl_tokens(struct_name, &fields.fk_relations);

    quote! {
        impl #struct_name {
            /// Start a new `QuerySet` over this model.
            #[must_use]
            pub fn objects() -> ::rustango::query::QuerySet<#struct_name> {
                ::rustango::query::QuerySet::new()
            }

            #insert_method

            #bulk_insert_method

            #save_method

            #pk_methods

            #pk_value_helper

            #aliased_row_helper

            #column_consts
        }

        #load_related_impl

        #has_pk_value_impl

        #fk_pk_access_impl
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
    quote! {
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
}

/// Parsed form of one index declaration (field-level or container-level).
struct IndexAttr {
    /// Index name; auto-derived when `None` at parse time.
    name: Option<String>,
    /// Column names in the index.
    columns: Vec<String>,
    /// `true` for `CREATE UNIQUE INDEX`.
    unique: bool,
}

/// Parsed form of one `#[rustango(check(name = "…", expr = "…"))]` declaration.
struct CheckAttr {
    name: String,
    expr: String,
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
        permissions: false,
        m2m: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
    };
    for attr in &input.attrs {
        if !attr.path().is_ident("rustango") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let s: LitStr = meta.value()?.parse()?;
                out.table = Some(s.value());
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
                out.permissions = true;
                return Ok(());
            }
            if meta.path.is_ident("index") {
                // Container-level composite index:
                // #[rustango(index("col1, col2"))]
                // #[rustango(index("col1, col2", unique, name = "my_idx"))]
                let cols_lit: LitStr = meta.value()?.parse()?;
                let columns = split_field_list(&cols_lit.value());
                let mut unique = false;
                let mut name: Option<String> = None;
                if meta.input.peek(syn::Token![,]) {
                    let _: syn::Token![,] = meta.input.parse()?;
                    // Parse remaining k=v or bare flags after the columns string
                    meta.parse_nested_meta(|inner| {
                        if inner.path.is_ident("unique") {
                            unique = true;
                            return Ok(());
                        }
                        if inner.path.is_ident("name") {
                            let s: LitStr = inner.value()?.parse()?;
                            name = Some(s.value());
                            return Ok(());
                        }
                        Err(inner.error("unknown index attribute (supported: `unique`, `name`)"))
                    })?;
                }
                out.indexes.push(IndexAttr { name, columns, unique });
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
                Some((title, rest)) if !title.contains(',') => {
                    (title.trim().to_owned(), rest)
                }
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
                .map_or((spec.to_owned(), false), |rest| (rest.trim().to_owned(), true))
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
    };
    for attr in &field.attrs {
        if !attr.path().is_ident("rustango") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("column") {
                let s: LitStr = meta.value()?.parse()?;
                out.column = Some(s.value());
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
                // Optional sub-attrs: #[rustango(index(unique, name = "…"))]
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
                        Err(inner.error("unknown index sub-attribute (supported: `unique`, `name`)"))
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
    /// Inner type from a `ForeignKey<T>` field, if any. The reverse-
    /// relation helper emit (`Author::<child>_set`) needs to know `T`
    /// to point the generated method at the right child model.
    fk_inner: Option<Type>,
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
}

fn process_field(field: &syn::Field) -> syn::Result<FieldInfo<'_>> {
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
    let relation = relation_tokens(field, &attrs, fk_inner)?;
    let column_lit = column.as_str();
    let field_type_tokens = kind.variant_tokens();
    let max_length = optional_u32(attrs.max_length);
    let min = optional_i64(attrs.min);
    let max = optional_i64(attrs.max);
    let default = optional_str(attrs.default.as_deref());

    let unique = attrs.unique;
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
        auto_now: attrs.auto_now,
        auto_now_add: attrs.auto_now_add,
        soft_delete: attrs.soft_delete,
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

fn relation_tokens(
    field: &syn::Field,
    attrs: &FieldAttrs,
    fk_inner: Option<&syn::Type>,
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
            Ok(quote! {
                ::core::option::Option::Some(::rustango::core::Relation::Fk { to: #to, on: #on })
            })
        }
        (None, Some(to)) => {
            let on = attrs.on.as_deref().unwrap_or("id");
            Ok(quote! {
                ::core::option::Option::Some(::rustango::core::Relation::O2O { to: #to, on: #on })
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
        matches!(self, Self::I32 | Self::I64)
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
            return Err(syn::Error::new_spanned(
                ty,
                "nested Auto is not supported",
            ));
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
        let inner = generic_inner(ty, &last.arguments, "ForeignKey")?;
        // `ForeignKey<T>` is stored as BIGINT — same column shape as
        // the v0.1 `i64` + `#[rustango(fk = …)]` form. The macro does
        // not recurse into `T` because `T` is a Model struct, not a
        // primitive — its identity is opaque to schema detection.
        return Ok(DetectedType {
            kind: DetectedKind::I64,
            nullable: false,
            auto: false,
            fk_inner: Some(inner),
        });
    }

    let kind = match last.ident.to_string().as_str() {
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
    I32,
    I64,
    F32,
    F64,
    Bool,
}

impl FormFieldKind {
    fn parse_method(self) -> &'static str {
        match self {
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
                     i32 / i64 / f32 / f64 / bool, optionally wrapped in Option<…>"
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
        FormFieldKind::I32 | FormFieldKind::I64 | FormFieldKind::F32 | FormFieldKind::F64 => {
            let parse_ty = syn::Ident::new(kind.parse_method(), proc_macro2::Span::call_site());
            let ty_lit = kind.parse_method();
            let default_val = match kind {
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
        FormFieldKind::I32 | FormFieldKind::I64 | FormFieldKind::F32 | FormFieldKind::F64
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
                ::rustango::viewset::ViewSet::for_model(#model_path::SCHEMA)
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
        syn::Error::new_spanned(
            &input.ident,
            "`#[viewset(model = SomeModel)]` is required",
        )
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
            Err(meta.error(
                "unknown serializer field attribute \
                 (supported: `read_only`, `write_only`, `source`, `skip`)",
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

    // Classify each field
    struct FieldInfo {
        ident: syn::Ident,
        attrs: SerializerFieldAttrs,
    }
    let mut fields_info: Vec<FieldInfo> = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().expect("named field has ident");
        let attrs = parse_serializer_field_attrs(field)?;
        fields_info.push(FieldInfo { ident, attrs });
    }

    // Generate from_model body: struct literal with each field assigned.
    let from_model_fields = fields_info.iter().map(|fi| {
        let ident = &fi.ident;
        if fi.attrs.write_only || fi.attrs.skip {
            // Not read from model — use default
            quote! { #ident: ::core::default::Default::default() }
        } else if let Some(src) = &fi.attrs.source {
            let src_ident = syn::Ident::new(src, ident.span());
            quote! { #ident: ::core::clone::Clone::clone(&model.#src_ident) }
        } else {
            quote! { #ident: ::core::clone::Clone::clone(&model.#ident) }
        }
    });

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

    // writable_fields: normal + write_only (not read_only, not skip)
    let writable_lits: Vec<_> = fields_info
        .iter()
        .filter(|fi| !fi.attrs.read_only && !fi.attrs.skip)
        .map(|fi| fi.ident.to_string())
        .collect();

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
    })
}
