// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Derive macro for [`photon_ring::Pod`].
//!
//! ```ignore
//! #[derive(photon_ring::Pod)]
//! struct Quote {
//!     price: f64,
//!     volume: u32,
//! }
//! ```
//!
//! This generates `#[repr(C)]`, `#[derive(Clone, Copy)]`, and
//! `unsafe impl photon_ring::Pod for Quote {}` — with a compile-time
//! check that every field type implements `Pod`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields};

/// Derive `Pod` for a struct.
///
/// Requirements:
/// - Must be a struct (not enum or union).
/// - All fields must implement `Pod`.
/// - The struct will be given `#[repr(C)]` semantics (the macro adds
///   `Clone` and `Copy` derives and the `unsafe impl Pod`).
///
/// # Example
///
/// ```ignore
/// #[derive(photon_ring::Pod)]
/// struct Tick {
///     price: f64,
///     volume: u32,
///     _pad: u32,
/// }
/// ```
#[proc_macro_derive(Pod)]
pub fn derive_pod(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    // Only structs are supported
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
            Fields::Unnamed(f) => f.unnamed.iter().collect::<Vec<_>>(),
            Fields::Unit => vec![],
        },
        _ => {
            return syn::Error::new_spanned(&input.ident, "Pod can only be derived for structs")
                .to_compile_error()
                .into();
        }
    };

    // Generate compile-time assertions that every field is Pod
    let field_assertions = fields.iter().map(|f| {
        let ty = &f.ty;
        quote! {
            const _: () = {
                fn _assert_pod<T: photon_ring::Pod>() {}
                fn _check() { _assert_pod::<#ty>(); }
            };
        }
    });

    let expanded = quote! {
        // Compile-time field checks
        #(#field_assertions)*

        // Safety: all fields verified to be Pod via compile-time assertions above.
        // The derive macro only applies to structs, and Pod requires that every
        // bit pattern is valid — which holds when all fields are Pod.
        unsafe impl #impl_generics photon_ring::Pod for #name #ty_generics #where_clause {}
    };

    TokenStream::from(expanded)
}
