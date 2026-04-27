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

    let table =
        parse_container_attrs(input)?.unwrap_or_else(|| to_snake_case(&struct_name.to_string()));
    let model_name = struct_name.to_string();

    let collected = collect_fields(named)?;

    let model_impl = model_impl_tokens(struct_name, &model_name, &table, &collected.field_schemas);
    let module_ident = column_module_ident(struct_name);
    let column_consts = column_const_tokens(&module_ident, &collected.column_entries);
    let inherent_impl = inherent_impl_tokens(
        struct_name,
        &collected.insert_columns,
        &collected.insert_values,
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
    insert_columns: Vec<TokenStream2>,
    insert_values: Vec<TokenStream2>,
    primary_key: Option<(syn::Ident, String)>,
    column_entries: Vec<ColumnEntry>,
}

fn collect_fields(named: &syn::FieldsNamed) -> syn::Result<CollectedFields> {
    let cap = named.named.len();
    let mut out = CollectedFields {
        field_schemas: Vec::with_capacity(cap),
        from_row_inits: Vec::with_capacity(cap),
        insert_columns: Vec::with_capacity(cap),
        insert_values: Vec::with_capacity(cap),
        primary_key: None,
        column_entries: Vec::with_capacity(cap),
    };

    for field in &named.named {
        let info = process_field(field)?;
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
    field_schemas: &[TokenStream2],
) -> TokenStream2 {
    quote! {
        impl ::rustango::core::Model for #struct_name {
            const SCHEMA: &'static ::rustango::core::ModelSchema = &::rustango::core::ModelSchema {
                name: #model_name,
                table: #table,
                fields: &[ #(#field_schemas),* ],
            };
        }
    }
}

fn inherent_impl_tokens(
    struct_name: &syn::Ident,
    insert_columns: &[TokenStream2],
    insert_values: &[TokenStream2],
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

    quote! {
        impl #struct_name {
            /// Start a new `QuerySet` over this model.
            #[must_use]
            pub fn objects() -> ::rustango::query::QuerySet<#struct_name> {
                ::rustango::query::QuerySet::new()
            }

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
                };
                ::rustango::sql::insert(pool, &query).await
            }

            #pk_methods

            #column_consts
        }
    }
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

fn parse_container_attrs(input: &DeriveInput) -> syn::Result<Option<String>> {
    let mut table = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("rustango") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let s: LitStr = meta.value()?.parse()?;
                table = Some(s.value());
                return Ok(());
            }
            Err(meta.error("unknown rustango container attribute"))
        })?;
    }
    Ok(table)
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
    let (kind, nullable) = detect_type(&field.ty)?;
    check_bound_compatibility(field, &attrs, kind)?;
    let relation = relation_tokens(field, &attrs)?;
    let column_lit = column.as_str();
    let field_type_tokens = kind.variant_tokens();
    let max_length = optional_u32(attrs.max_length);
    let min = optional_i64(attrs.min);
    let max = optional_i64(attrs.max);

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
        }
    };

    let from_row_init = quote! {
        #ident: ::rustango::sql::sqlx::Row::try_get(row, #column_lit)?
    };

    Ok(FieldInfo {
        ident,
        column,
        primary_key,
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

fn detect_type(ty: &Type) -> syn::Result<(DetectedKind, bool)> {
    let Type::Path(TypePath { path, qself: None }) = ty else {
        return Err(syn::Error::new_spanned(ty, "unsupported field type"));
    };
    let last = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "empty type path"))?;

    if last.ident == "Option" {
        let PathArguments::AngleBracketed(args) = &last.arguments else {
            return Err(syn::Error::new_spanned(
                ty,
                "Option requires a generic argument",
            ));
        };
        let inner = args
            .args
            .iter()
            .find_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .ok_or_else(|| syn::Error::new_spanned(ty, "Option<T> requires a type argument"))?;
        let (kind, already_nullable) = detect_type(inner)?;
        if already_nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "nested Option is not supported",
            ));
        }
        return Ok((kind, true));
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
                format!("unsupported field type `{other}`; v0.1 supports i32/i64/f32/f64/bool/String/DateTime/NaiveDate/Uuid/serde_json::Value, optionally wrapped in Option"),
            ));
        }
    };
    Ok((kind, false))
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
