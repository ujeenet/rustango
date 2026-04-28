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

    let model_impl = model_impl_tokens(
        struct_name,
        &model_name,
        &table,
        display.as_deref(),
        &collected.field_schemas,
    );
    let module_ident = column_module_ident(struct_name);
    let column_consts = column_const_tokens(&module_ident, &collected.column_entries);
    let inherent_impl = inherent_impl_tokens(
        struct_name,
        &collected,
        collected.primary_key.as_ref(),
        &column_consts,
    );
    let column_module = column_module_tokens(&module_ident, struct_name, &collected.column_entries);
    let from_row_impl = from_row_impl_tokens(struct_name, &collected.from_row_inits);

    Ok(quote! {
        #model_impl
        #inherent_impl
        #from_row_impl
        #column_module

        ::rustango::core::inventory::submit! {
            ::rustango::core::ModelEntry {
                schema: <#struct_name as ::rustango::core::Model>::SCHEMA,
            }
        }
    })
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
    primary_key: Option<(syn::Ident, String)>,
    column_entries: Vec<ColumnEntry>,
    /// Rust-side field names, in declaration order. Used to validate
    /// container attributes like `display = "…"`.
    field_names: Vec<String>,
}

fn collect_fields(named: &syn::FieldsNamed) -> syn::Result<CollectedFields> {
    let cap = named.named.len();
    let mut out = CollectedFields {
        field_schemas: Vec::with_capacity(cap),
        from_row_inits: Vec::with_capacity(cap),
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
        primary_key: None,
        column_entries: Vec::with_capacity(cap),
        field_names: Vec::with_capacity(cap),
    };

    for field in &named.named {
        let info = process_field(field)?;
        out.field_names.push(info.ident.to_string());
        out.field_schemas.push(info.schema);
        out.from_row_inits.push(info.from_row_init);
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
    field_schemas: &[TokenStream2],
) -> TokenStream2 {
    let display_tokens = if let Some(name) = display {
        quote!(::core::option::Option::Some(#name))
    } else {
        quote!(::core::option::Option::None)
    };
    quote! {
        impl ::rustango::core::Model for #struct_name {
            const SCHEMA: &'static ::rustango::core::ModelSchema = &::rustango::core::ModelSchema {
                name: #model_name,
                table: #table,
                fields: &[ #(#field_schemas),* ],
                display: #display_tokens,
            };
        }
    }
}

fn inherent_impl_tokens(
    struct_name: &syn::Ident,
    fields: &CollectedFields,
    primary_key: Option<&(syn::Ident, String)>,
    column_consts: &TokenStream2,
) -> TokenStream2 {
    let pk_methods = primary_key.map(|(pk_ident, pk_column)| {
        let pk_column_lit = pk_column.as_str();
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
                let query = ::rustango::core::DeleteQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    filters: ::std::vec![
                        ::rustango::core::Filter {
                            column: #pk_column_lit,
                            op: ::rustango::core::Op::Eq,
                            value: ::core::convert::Into::<::rustango::core::SqlValue>::into(
                                ::core::clone::Clone::clone(&self.#pk_ident)
                            ),
                        }
                    ],
                };
                ::rustango::sql::delete(pool, &query).await
            }
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
                };
                let _returning_row = ::rustango::sql::insert_returning(pool, &query).await?;
                #( #auto_assigns )*
                ::core::result::Result::Ok(())
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
                let query = ::rustango::core::InsertQuery {
                    model: <Self as ::rustango::core::Model>::SCHEMA,
                    columns: ::std::vec![ #( #insert_columns ),* ],
                    values: ::std::vec![ #( #insert_values ),* ],
                    returning: ::std::vec::Vec::new(),
                };
                ::rustango::sql::insert(pool, &query).await
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
                };
                let _returned = ::rustango::sql::bulk_insert(pool, &_query).await?;
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
                };
                let _ = ::rustango::sql::bulk_insert(pool, &_query).await?;
                ::core::result::Result::Ok(())
            }
        }
    };

    quote! {
        impl #struct_name {
            /// Start a new `QuerySet` over this model.
            #[must_use]
            pub fn objects() -> ::rustango::query::QuerySet<#struct_name> {
                ::rustango::query::QuerySet::new()
            }

            #insert_method

            #bulk_insert_method

            #pk_methods

            #column_consts
        }
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
}

fn parse_container_attrs(input: &DeriveInput) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs {
        table: None,
        display: None,
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
            Err(meta.error("unknown rustango container attribute"))
        })?;
    }
    Ok(out)
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
        auto,
    } = detect_type(&field.ty)?;
    check_bound_compatibility(field, &attrs, kind)?;
    if auto && !primary_key {
        return Err(syn::Error::new_spanned(
            field,
            "`Auto<T>` is only valid on a `#[rustango(primary_key)]` field",
        ));
    }
    if auto && attrs.default.is_some() {
        return Err(syn::Error::new_spanned(
            field,
            "`#[rustango(default = \"…\")]` is redundant on an `Auto<T>` field — \
             SERIAL / BIGSERIAL already supplies a default sequence.",
        ));
    }
    let relation = relation_tokens(field, &attrs)?;
    let column_lit = column.as_str();
    let field_type_tokens = kind.variant_tokens();
    let max_length = optional_u32(attrs.max_length);
    let min = optional_i64(attrs.min);
    let max = optional_i64(attrs.max);
    let default = optional_str(attrs.default.as_deref());

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
        }
    };

    let from_row_init = quote! {
        #ident: ::rustango::sql::sqlx::Row::try_get(row, #column_lit)?
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

fn relation_tokens(field: &syn::Field, attrs: &FieldAttrs) -> syn::Result<TokenStream2> {
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
/// set by an outer `Auto<T>` (server-assigned PK).
#[derive(Clone, Copy)]
struct DetectedType {
    kind: DetectedKind,
    nullable: bool,
    auto: bool,
}

fn detect_type(ty: &Type) -> syn::Result<DetectedType> {
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
        if !matches!(inner_det.kind, DetectedKind::I32 | DetectedKind::I64) {
            return Err(syn::Error::new_spanned(
                ty,
                "`Auto<T>` only supports integer types (`i32` → SERIAL, `i64` → BIGSERIAL)",
            ));
        }
        return Ok(DetectedType {
            auto: true,
            ..inner_det
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
