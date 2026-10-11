// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `#[hot]`: see `segment::tinshape::isa`, which re-exports it.
//!
//! On x86-64, `#[hot] fn name(..) -> R { body }` becomes three functions:
//!
//! - `name`, with the original's signature, documentation and attributes
//!   (`#[inline(never)]`, `#[cold]`, ...): `if isa::is_v3() { name_v3(..) }
//!   else { name_portable(..) }`;
//! - `name_portable` (`#[inline(always)]`): `body`, compiled for the
//!   build's target;
//! - `name_v3`: `body` again, compiled with x86-64-v3's features
//!   (`#[target_feature]`), so everything it inlines is too.
//!
//! The body is compiled twice rather than inlined into a `#[target_feature]`
//! function: LLVM declines to inline code into a function with more
//! features when it calls something out of line with an aggregate argument
//! or result (a `Result<Option<u32>>`, a slice pair), which most of the
//! walk's bodies do.
//!
//! On every other target the function is left as written. The expansion
//! names `isa::is_v3`, so `segment::tinshape::isa` must be in scope as
//! `isa` where it is used. A function without a receiver is called by its
//! bare name, so it must be a free function, not an associated one.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Attribute, FnArg, ItemFn, Pat, PatIdent, parse_macro_input};

/// The target features of the v3 copies: x86-64-v3's integer extensions
/// and AVX2, without FMA (no float result may change).
const V3_FEATURES: &str = "popcnt,bmi1,bmi2,lzcnt,avx2";

/// Whether `attr` belongs on the copies too (lint levels), not only on the
/// dispatching function.
fn for_copies(attr: &Attribute) -> bool {
    let path = attr.path();
    ["allow", "expect", "warn", "deny"]
        .iter()
        .any(|name| path.is_ident(name))
}

#[proc_macro_attribute]
pub fn hot(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[hot] takes no arguments")
            .to_compile_error()
            .into();
    }
    let original = parse_macro_input!(item as ItemFn);
    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = &original;
    let name = &sig.ident;
    let portable = format_ident!("{}_portable", name);
    let v3 = format_ident!("{}_v3", name);
    let lints: Vec<&Attribute> = attrs.iter().filter(|a| for_copies(a)).collect();

    // The dispatcher's parameters: each typed one bound to a plain name.
    let mut outer = sig.clone();
    let mut args = Vec::new();
    let mut method = false;
    for (i, input) in outer.inputs.iter_mut().enumerate() {
        match input {
            FnArg::Receiver(_) => method = true,
            FnArg::Typed(typed) => {
                let ident = format_ident!("__hot_arg{}", i);
                *typed.pat = Pat::Ident(PatIdent {
                    attrs: Vec::new(),
                    by_ref: None,
                    mutability: None,
                    ident: ident.clone(),
                    subpat: None,
                });
                args.push(ident);
            }
        }
    }
    let (call_portable, call_v3) = if method {
        (
            quote!(self.#portable(#(#args),*)),
            quote!(self.#v3(#(#args),*)),
        )
    } else {
        (quote!(#portable(#(#args),*)), quote!(#v3(#(#args),*)))
    };

    let mut sig_portable = sig.clone();
    sig_portable.ident = portable;
    let mut sig_v3 = sig.clone();
    sig_v3.ident = v3;

    quote! {
        #[cfg(not(target_arch = "x86_64"))]
        #original

        #[cfg(target_arch = "x86_64")]
        #(#attrs)*
        #vis #outer {
            if isa::is_v3() {
                // SAFETY: `is_v3` holds only when the CPU has every
                // feature the v3 copy is compiled with.
                unsafe { #call_v3 }
            } else {
                #call_portable
            }
        }

        #[cfg(target_arch = "x86_64")]
        #(#lints)*
        #[inline(always)]
        #sig_portable #block

        #[cfg(target_arch = "x86_64")]
        #(#lints)*
        #[target_feature(enable = #V3_FEATURES)]
        #[inline]
        #sig_v3 #block
    }
    .into()
}
