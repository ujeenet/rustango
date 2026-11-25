//! `#[rustango::data_step(app = "...", id = "...")]` on an `impl` block with
//! `async fn up` (and optionally `async fn down`): registers the step for
//! engine v2 `data_rust` ops, keyed `(app, id)`, with its source for the
//! code hash.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::Parser as _;

pub(crate) fn expand(args: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let mut app: Option<syn::LitStr> = None;
    let mut id: Option<syn::LitStr> = None;
    let parser = syn::meta::parser(|meta| {
        let lit: syn::LitStr = meta.value()?.parse()?;
        if meta.path.is_ident("app") {
            app = Some(lit);
        } else if meta.path.is_ident("id") {
            id = Some(lit);
        } else {
            return Err(meta.error("expected `app` or `id`"));
        }
        Ok(())
    });
    parser.parse2(args)?;
    let span = proc_macro2::Span::call_site();
    let app = app.ok_or_else(|| syn::Error::new(span, "`data_step` needs `app = \"...\"`"))?;
    let id = id.ok_or_else(|| syn::Error::new(span, "`data_step` needs `id = \"...\"`"))?;
    let imp: syn::ItemImpl = syn::parse2(item)?;
    let syn::Type::Path(ty) = &*imp.self_ty else {
        return Err(syn::Error::new_spanned(
            &imp.self_ty,
            "`data_step` goes on `impl Name`",
        ));
    };
    let name = ty
        .path
        .get_ident()
        .ok_or_else(|| syn::Error::new_spanned(ty, "`data_step` goes on `impl Name`"))?
        .clone();
    let has = |f: &str| {
        imp.items.iter().any(
            |i| matches!(i, syn::ImplItem::Fn(m) if m.sig.ident == f && m.sig.asyncness.is_some()),
        )
    };
    if !has("up") {
        return Err(syn::Error::new_spanned(
            &name,
            "a data step needs `async fn up(ctx, batch)`",
        ));
    }
    let root = crate::rustango_root();
    let v2 = quote!(#root::migrate::v2);
    // The impl's own tokens: what the code hash covers (helpers it calls are not).
    let code = quote!(#imp).to_string();
    let down = if has("down") {
        quote! {{
            fn __rustango_down<'a, 'b>(
                ctx: &'a mut #v2::StepCtx<'b>,
                batch: #v2::Batch,
            ) -> #v2::StepFuture<'a> {
                ::std::boxed::Box::pin(#name::down(ctx, batch))
            }
            ::core::option::Option::Some(__rustango_down as #v2::StepFn)
        }}
    } else {
        quote!(::core::option::Option::None)
    };
    let name_str = name.to_string();
    Ok(quote! {
        #[allow(dead_code)]
        pub struct #name;

        #imp

        const _: () = {
            fn __rustango_up<'a, 'b>(
                ctx: &'a mut #v2::StepCtx<'b>,
                batch: #v2::Batch,
            ) -> #v2::StepFuture<'a> {
                ::std::boxed::Box::pin(#name::up(ctx, batch))
            }
            #root::inventory::submit! {
                #v2::DataStep {
                    app: #app,
                    id: #id,
                    type_name: ::core::concat!(::core::module_path!(), "::", #name_str),
                    up: __rustango_up,
                    down: #down,
                    code: #code,
                }
            }
        };
    })
}
