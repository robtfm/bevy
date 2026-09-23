use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, ForeignItem, ItemForeignMod, LitStr, ReturnType};

pub fn page_functions(item: TokenStream) -> TokenStream {
    let block = parse_macro_input!(item as ItemForeignMod);
    // The relays resolve with a promise, so a sync declaration returning a value would get a
    // `Promise` where it expects its value.
    for item in &block.items {
        if let ForeignItem::Fn(function) = item
            && function.sig.asyncness.is_none()
            && let ReturnType::Type(_, ty) = &function.sig.output
            && !is_promise(ty)
        {
            return syn::Error::new_spanned(
                ty,
                "page functions are relayed asynchronously: declare the function `async` or return `js_sys::Promise`",
            )
            .to_compile_error()
            .into();
        }
    }
    let names: Vec<String> = block
        .items
        .iter()
        .filter_map(|item| match item {
            ForeignItem::Fn(function) => Some(
                js_name(&function.attrs).unwrap_or_else(|| function.sig.ident.to_string()),
            ),
            _ => None,
        })
        .collect();

    // One export per attributed block, named for the crate and the block so blocks never
    // collide; the worker script finds them all by the prefix.
    let crate_name = std::env::var("CARGO_CRATE_NAME").unwrap_or_else(|_| "unknown".into());
    let hash = fnv1a(&format!("{crate_name}{}", quote!(#block)));
    let export = format!("__bevy_page_functions_{crate_name}_{hash:08x}");
    let export_ident = format_ident!("{export}");
    let export_name = LitStr::new(&export, proc_macro2::Span::call_site());
    let name_literals = names
        .iter()
        .map(|name| LitStr::new(name, proc_macro2::Span::call_site()));

    quote! {
        #block

        #[doc(hidden)]
        #[::wasm_bindgen::prelude::wasm_bindgen(js_name = #export_name)]
        pub fn #export_ident() -> ::std::vec::Vec<::std::string::String> {
            ::std::vec![#(::std::string::String::from(#name_literals)),*]
        }
    }
    .into()
}

/// The `js_name` of a `#[wasm_bindgen(...)]` attribute, given as a string or an identifier.
fn js_name(attrs: &[syn::Attribute]) -> Option<String> {
    let mut found = None;
    for attr in attrs {
        let is_wasm_bindgen = attr
            .path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "wasm_bindgen");
        if !is_wasm_bindgen {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("js_name") {
                let value = meta.value()?;
                found = Some(if value.peek(LitStr) {
                    value.parse::<LitStr>()?.value()
                } else {
                    value.parse::<syn::Ident>()?.to_string()
                });
            } else if meta.input.peek(syn::Token![=]) {
                // Another `key = value` (`js_namespace = self`, say): consume the value.
                meta.value()?.parse::<proc_macro2::TokenTree>()?;
            }
            Ok(())
        });
    }
    found
}

fn is_promise(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Promise"))
}

fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}
