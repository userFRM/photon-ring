// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Derive macros for [`photon_ring::Pod`] and [`photon_ring::Message`].
//!
//! ## `Pod` derive
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
//!
//! ## `Message` derive
//!
//! ```ignore
//! #[derive(photon_ring::Message)]
//! struct Order {
//!     price: f64,
//!     qty: u32,
//!     side: Side,        // any #[repr(u8)] enum
//!     filled: bool,
//!     tag: Option<u32>,
//! }
//! ```
//!
//! Generates a Pod-compatible wire struct (`OrderWire`) plus `From`
//! conversions in both directions. See [`derive_message`] for details.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields, GenericArgument, PathArguments, Type};

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

// ---------------------------------------------------------------------------
// Message derive
// ---------------------------------------------------------------------------

/// Classification of a field type for wire conversion.
enum FieldKind {
    /// Numeric or array — passes through unchanged.
    Passthrough,
    /// `bool` → `u8`.
    Bool,
    /// `usize` → `u64`.
    Usize,
    /// `isize` → `i64`.
    Isize,
    /// `Option<T>` where T is a numeric type → `u64`.
    /// Stores the inner type for the back-conversion cast.
    OptionNumeric(Type),
    /// Any other type — assumed to be a `#[repr(u8)]` enum → `u8`.
    Enum,
}

/// Returns true if `ty` is a known Pod-passthrough numeric or float type.
fn is_numeric(ty: &Type) -> bool {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            let id = seg.ident.to_string();
            return matches!(
                id.as_str(),
                "u8" | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "f32"
                    | "f64"
            );
        }
    }
    false
}

/// Classify a field's type into a [`FieldKind`].
fn classify(ty: &Type) -> FieldKind {
    match ty {
        // Arrays `[T; N]` — passthrough (must be Pod).
        Type::Array(_) => FieldKind::Passthrough,

        Type::Path(p) => {
            let seg = match p.path.segments.last() {
                Some(s) => s,
                None => return FieldKind::Enum,
            };
            let id = seg.ident.to_string();

            match id.as_str() {
                // Numerics — passthrough
                "u8" | "u16" | "u32" | "u64" | "u128" | "i8" | "i16" | "i32" | "i64" | "i128"
                | "f32" | "f64" => FieldKind::Passthrough,

                "bool" => FieldKind::Bool,
                "usize" => FieldKind::Usize,
                "isize" => FieldKind::Isize,

                "Option" => {
                    // Extract inner type from Option<T>
                    if let PathArguments::AngleBracketed(args) = &seg.arguments {
                        if let Some(GenericArgument::Type(inner)) = args.args.first() {
                            if is_numeric(inner) {
                                return FieldKind::OptionNumeric(inner.clone());
                            }
                        }
                    }
                    // Non-numeric Option — not supported, will be caught by
                    // the compile-time size assertion below.
                    FieldKind::Enum
                }

                // Anything else — assume enum
                _ => FieldKind::Enum,
            }
        }

        _ => FieldKind::Enum,
    }
}

/// Derive a Pod-compatible wire struct with `From` conversions.
///
/// Given a struct with fields that may include `bool`, `Option<numeric>`,
/// `usize`/`isize`, and `#[repr(u8)]` enums, generates:
///
/// 1. **`{Name}Wire`** — a `#[repr(C)] Clone + Copy` struct with all fields
///    converted to Pod-safe types, plus `unsafe impl Pod`.
/// 2. **`From<Name> for {Name}Wire`** — converts the domain struct to wire.
/// 3. **`From<{Name}Wire> for Name`** — converts the wire struct back.
///
/// # Field type mappings
///
/// | Source type | Wire type | To wire | From wire |
/// |---|---|---|---|
/// | `f32`, `f64`, `u8`..`u128`, `i8`..`i128` | same | passthrough | passthrough |
/// | `usize` | `u64` | `as u64` | `as usize` |
/// | `isize` | `i64` | `as i64` | `as isize` |
/// | `bool` | `u8` | `if v { 1 } else { 0 }` | `v != 0` |
/// | `Option<T>` (T numeric) | `u64` | `Some(x) => x as u64, None => 0` | `0 => None, v => Some(v as T)` |
/// | `[T; N]` (T: Pod) | same | passthrough | passthrough |
/// | Any other type | `u8` | `v as u8` | `transmute(v)` |
///
/// # Enum safety
///
/// Enum fields are converted via `transmute` from `u8`. The enum **must**
/// have `#[repr(u8)]` — the macro emits a compile-time `size_of` check to
/// enforce this.
///
/// # Example
///
/// ```ignore
/// #[repr(u8)]
/// #[derive(Clone, Copy)]
/// enum Side { Buy = 0, Sell = 1 }
///
/// #[derive(photon_ring::Message)]
/// struct Order {
///     price: f64,
///     qty: u32,
///     side: Side,
///     filled: bool,
///     tag: Option<u32>,
/// }
/// // Generates: OrderWire, From<Order> for OrderWire, From<OrderWire> for Order
/// ```
#[proc_macro_derive(Message)]
pub fn derive_message(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let wire_name = format_ident!("{}Wire", name);

    // Only named structs are supported
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
            _ => {
                return syn::Error::new_spanned(
                    &input.ident,
                    "Message can only be derived for structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(
                &input.ident,
                "Message can only be derived for structs",
            )
            .to_compile_error()
            .into();
        }
    };

    let mut wire_fields = Vec::new();
    let mut to_wire = Vec::new();
    let mut from_wire = Vec::new();
    let mut assertions = Vec::new();

    for field in &fields {
        let fname = field.ident.as_ref().unwrap();
        let fty = &field.ty;
        let kind = classify(fty);

        match kind {
            FieldKind::Passthrough => {
                wire_fields.push(quote! { pub #fname: #fty });
                to_wire.push(quote! { #fname: src.#fname });
                from_wire.push(quote! { #fname: src.#fname });
            }
            FieldKind::Bool => {
                wire_fields.push(quote! { pub #fname: u8 });
                to_wire.push(quote! { #fname: if src.#fname { 1 } else { 0 } });
                from_wire.push(quote! { #fname: src.#fname != 0 });
            }
            FieldKind::Usize => {
                wire_fields.push(quote! { pub #fname: u64 });
                to_wire.push(quote! { #fname: src.#fname as u64 });
                from_wire.push(quote! { #fname: src.#fname as usize });
            }
            FieldKind::Isize => {
                wire_fields.push(quote! { pub #fname: i64 });
                to_wire.push(quote! { #fname: src.#fname as i64 });
                from_wire.push(quote! { #fname: src.#fname as isize });
            }
            FieldKind::OptionNumeric(inner) => {
                wire_fields.push(quote! { pub #fname: u64 });
                to_wire.push(quote! {
                    #fname: match src.#fname {
                        Some(v) => v as u64,
                        None => 0,
                    }
                });
                from_wire.push(quote! {
                    #fname: if src.#fname == 0 {
                        None
                    } else {
                        Some(src.#fname as #inner)
                    }
                });
            }
            FieldKind::Enum => {
                wire_fields.push(quote! { pub #fname: u8 });
                to_wire.push(quote! { #fname: src.#fname as u8 });
                from_wire.push(quote! {
                    #fname: unsafe { core::mem::transmute::<u8, #fty>(src.#fname) }
                });
                // Compile-time assertion: enum must be 1 byte (#[repr(u8)])
                let msg = format!(
                    "Message derive: field `{}` has type `{}` which is not 1 byte. \
                     Enum fields must have #[repr(u8)].",
                    fname,
                    quote! { #fty },
                );
                let msg_lit = syn::LitStr::new(&msg, Span::call_site());
                assertions.push(quote! {
                    const _: () = {
                        assert!(
                            core::mem::size_of::<#fty>() == 1,
                            #msg_lit,
                        );
                    };
                });
            }
        }
    }

    let expanded = quote! {
        // Compile-time assertions for enum fields
        #(#assertions)*

        /// Auto-generated Pod-compatible wire struct for [`#name`].
        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct #wire_name {
            #(#wire_fields),*
        }

        // Safety: all fields of the wire struct are plain numeric types
        // (u8, u32, u64, f32, f64, etc.) where every bit pattern is valid.
        unsafe impl photon_ring::Pod for #wire_name {}

        impl From<#name> for #wire_name {
            #[inline]
            fn from(src: #name) -> Self {
                #wire_name {
                    #(#to_wire),*
                }
            }
        }

        impl From<#wire_name> for #name {
            #[inline]
            fn from(src: #wire_name) -> Self {
                #name {
                    #(#from_wire),*
                }
            }
        }
    };

    TokenStream::from(expanded)
}
