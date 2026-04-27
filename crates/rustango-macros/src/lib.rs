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

    let mut field_schemas: Vec<TokenStream2> = Vec::with_capacity(named.named.len());
    let mut from_row_inits: Vec<TokenStream2> = Vec::with_capacity(named.named.len());
    let mut insert_columns: Vec<TokenStream2> = Vec::with_capacity(named.named.len());
    let mut insert_values: Vec<TokenStream2> = Vec::with_capacity(named.named.len());
    for field in &named.named {
        let info = process_field(field)?;
        field_schemas.push(info.schema);
        from_row_inits.push(info.from_row_init);
        let column = &info.column;
        let ident = info.ident;
        insert_columns.push(quote!(#column));
        insert_values.push(quote! {
            ::core::convert::Into::<::rustango::core::SqlValue>::into(
                ::core::clone::Clone::clone(&self.#ident)
            )
        });
    }

    Ok(quote! {
        impl ::rustango::core::Model for #struct_name {
            const SCHEMA: &'static ::rustango::core::ModelSchema = &::rustango::core::ModelSchema {
                name: #model_name,
                table: #table,
                fields: &[ #(#field_schemas),* ],
            };
        }

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
        }

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

        ::rustango::core::inventory::submit! {
            ::rustango::core::ModelEntry {
                schema: <#struct_name as ::rustango::core::Model>::SCHEMA,
            }
        }
    })
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
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs {
        column: None,
        primary_key: false,
        fk: None,
        o2o: None,
        on: None,
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
            Err(meta.error("unknown rustango field attribute"))
        })?;
    }
    Ok(out)
}

struct FieldInfo<'a> {
    ident: &'a syn::Ident,
    column: String,
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
    let (ty_tokens, nullable) = detect_type(&field.ty)?;
    let relation = relation_tokens(field, &attrs)?;
    let column_lit = column.as_str();

    let schema = quote! {
        ::rustango::core::FieldSchema {
            name: #name,
            column: #column_lit,
            ty: #ty_tokens,
            nullable: #nullable,
            primary_key: #primary_key,
            relation: #relation,
        }
    };

    let from_row_init = quote! {
        #ident: ::rustango::sql::sqlx::Row::try_get(row, #column_lit)?
    };

    Ok(FieldInfo {
        ident,
        column,
        schema,
        from_row_init,
    })
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

fn detect_type(ty: &Type) -> syn::Result<(TokenStream2, bool)> {
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
        let (variant, already_nullable) = detect_type(inner)?;
        if already_nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "nested Option is not supported",
            ));
        }
        return Ok((variant, true));
    }

    let variant = match last.ident.to_string().as_str() {
        "i32" => quote!(::rustango::core::FieldType::I32),
        "i64" => quote!(::rustango::core::FieldType::I64),
        "f32" => quote!(::rustango::core::FieldType::F32),
        "f64" => quote!(::rustango::core::FieldType::F64),
        "bool" => quote!(::rustango::core::FieldType::Bool),
        "String" => quote!(::rustango::core::FieldType::String),
        "DateTime" => quote!(::rustango::core::FieldType::DateTime),
        "NaiveDate" => quote!(::rustango::core::FieldType::Date),
        "Uuid" => quote!(::rustango::core::FieldType::Uuid),
        "Value" => quote!(::rustango::core::FieldType::Json),
        other => {
            return Err(syn::Error::new_spanned(
                ty,
                format!("unsupported field type `{other}`; v0.1 supports i32/i64/f32/f64/bool/String/DateTime/NaiveDate/Uuid/serde_json::Value, optionally wrapped in Option"),
            ));
        }
    };
    Ok((variant, false))
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
