//! `#[derive(TextEnum)]`: a fieldless enum stored as one text value per variant.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr};

use crate::rustango_root;

/// How a variant ident becomes its stored value when no `rename` is given.
#[derive(Clone, Copy)]
enum RenameAll {
    AsIs,
    Lower,
    Snake,
    Kebab,
    ScreamingSnake,
}

impl RenameAll {
    fn parse(s: &LitStr) -> syn::Result<Self> {
        Ok(match s.value().as_str() {
            "lowercase" => Self::Lower,
            "snake_case" => Self::Snake,
            "kebab-case" => Self::Kebab,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            other => {
                return Err(syn::Error::new(
                    s.span(),
                    format!(
                        "unknown `rename_all = \"{other}\"`; expected `lowercase`, \
                         `snake_case`, `kebab-case` or `SCREAMING_SNAKE_CASE`"
                    ),
                ));
            }
        })
    }

    fn apply(self, ident: &str) -> String {
        let words = || {
            let mut out = String::new();
            for (i, c) in ident.chars().enumerate() {
                if c.is_uppercase() && i > 0 {
                    out.push('_');
                }
                out.extend(c.to_lowercase());
            }
            out
        };
        match self {
            Self::AsIs => ident.to_owned(),
            Self::Lower => ident.to_lowercase(),
            Self::Snake => words(),
            Self::Kebab => words().replace('_', "-"),
            Self::ScreamingSnake => words().to_uppercase(),
        }
    }
}

struct Variant {
    ident: syn::Ident,
    value: String,
    label: String,
    aliases: Vec<String>,
}

pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let root = rustango_root();
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`TextEnum` does not take generics",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "`TextEnum` derives only on an enum",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            name,
            "`TextEnum` needs at least one variant",
        ));
    }

    let mut rename_all = RenameAll::AsIs;
    for attr in input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("text_enum"))
    {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                rename_all = RenameAll::parse(&meta.value()?.parse()?)?;
                return Ok(());
            }
            Err(meta.error("unknown `text_enum` attribute; supported: `rename_all`"))
        })?;
    }

    let mut variants: Vec<Variant> = Vec::new();
    for v in &data.variants {
        if !matches!(v.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                v,
                "`TextEnum` variants carry no data; store data in other fields",
            ));
        }
        let ident_s = v.ident.to_string();
        let mut value = None;
        let mut label = None;
        let mut aliases = Vec::new();
        for attr in v.attrs.iter().filter(|a| a.path().is_ident("text_enum")) {
            attr.parse_nested_meta(|meta| {
                let lit: LitStr = meta.value()?.parse()?;
                if lit.value().is_empty() {
                    return Err(syn::Error::new(lit.span(), "a stored value cannot be empty"));
                }
                if meta.path.is_ident("rename") {
                    value = Some(lit.value());
                } else if meta.path.is_ident("alias") {
                    aliases.push(lit.value());
                } else if meta.path.is_ident("label") {
                    label = Some(lit.value());
                } else {
                    return Err(meta.error(
                        "unknown `text_enum` variant attribute; supported: `rename`, `alias`, `label`",
                    ));
                }
                Ok(())
            })?;
        }
        variants.push(Variant {
            value: value.unwrap_or_else(|| rename_all.apply(&ident_s)),
            label: label.unwrap_or_else(|| ident_s.clone()),
            ident: v.ident.clone(),
            aliases,
        });
    }

    // Every stored name and alias reads back to exactly one variant.
    let mut seen: Vec<&str> = Vec::new();
    for v in &variants {
        for s in std::iter::once(&v.value).chain(&v.aliases) {
            if seen.contains(&s.as_str()) {
                return Err(syn::Error::new_spanned(
                    &v.ident,
                    format!("`{s}` is used by two variants (as a stored name or alias)"),
                ));
            }
            seen.push(s);
        }
    }

    let name_lit = name.to_string();
    let entries = variants.iter().map(|v| {
        let (ident, value, label) = (&v.ident, &v.value, &v.label);
        quote!((Self::#ident, #value, #label))
    });
    let alias_entries = variants.iter().flat_map(|v| {
        let ident = &v.ident;
        v.aliases.iter().map(move |a| quote!((Self::#ident, #a)))
    });
    let as_str_arms = variants.iter().map(|v| {
        let (ident, value) = (&v.ident, &v.value);
        quote!(Self::#ident => #value)
    });
    let from_db_arms = variants.iter().map(|v| {
        let ident = &v.ident;
        let names = std::iter::once(&v.value).chain(&v.aliases);
        quote!(#(#names)|* => ::core::result::Result::Ok(Self::#ident))
    });
    let serde = quote!(#root::__serde);

    Ok(quote! {
        impl #root::core::TextEnum for #name {
            const NAME: &'static str = #name_lit;
            const VARIANTS: &'static [(Self, &'static str, &'static str)] = &[#(#entries),*];
            const ALIASES: &'static [(Self, &'static str)] = &[#(#alias_entries),*];

            fn as_str(&self) -> &'static str {
                match self { #(#as_str_arms,)* }
            }

            fn from_db(stored: &str) -> ::core::result::Result<Self, #root::core::UnknownVariant> {
                match stored {
                    #(#from_db_arms,)*
                    _ => ::core::result::Result::Err(#root::core::UnknownVariant::new::<Self>(stored)),
                }
            }
        }

        impl ::core::fmt::Display for #name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(<Self as #root::core::TextEnum>::as_str(self))
            }
        }

        impl ::core::str::FromStr for #name {
            type Err = #root::core::UnknownVariant;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                <Self as #root::core::TextEnum>::from_db(s)
            }
        }

        impl #serde::Serialize for #name {
            fn serialize<S: #serde::Serializer>(&self, s: S) -> ::core::result::Result<S::Ok, S::Error> {
                s.serialize_str(<Self as #root::core::TextEnum>::as_str(self))
            }
        }

        impl<'de> #serde::Deserialize<'de> for #name {
            fn deserialize<D: #serde::Deserializer<'de>>(d: D) -> ::core::result::Result<Self, D::Error> {
                let s = <::std::string::String as #serde::Deserialize>::deserialize(d)?;
                <Self as #root::core::TextEnum>::from_db(&s).map_err(<D::Error as #serde::de::Error>::custom)
            }
        }

        #root::__text_column_impls!([] #name, |stored| <#name as #root::core::TextEnum>::from_db(stored));
    })
}
