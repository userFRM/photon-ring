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
//! This generates compile-time assertions that every field implements `Pod`,
//! plus `unsafe impl photon_ring::Pod for Quote {}`.
//!
//! **Note:** The macro does *not* add `#[repr(C)]` or `Clone`/`Copy` derives.
//! You must add those yourself for the `Pod` contract to hold.
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
/// - The user must add `#[repr(C)]`, `Clone`, and `Copy` themselves;
///   the macro only emits field assertions and `unsafe impl Pod`.
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
    /// `Option<T>` where T is an integer type.
    /// Wire struct gets two fields: `X_value: u64` and `X_has: u8`.
    /// Stores the inner type for the back-conversion cast.
    OptionInt(Type),
    /// `Option<f32>` — wire struct gets `X_value: u64` (bit-encoded) and `X_has: u8`.
    OptionF32,
    /// `Option<f64>` — wire struct gets `X_value: u64` (bit-encoded) and `X_has: u8`.
    OptionF64,
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

/// Returns the type name string for a simple path type, or `None`.
fn type_name(ty: &Type) -> Option<String> {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            return Some(seg.ident.to_string());
        }
    }
    None
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
                                let name = type_name(inner).unwrap_or_default();
                                return match name.as_str() {
                                    "f32" => FieldKind::OptionF32,
                                    "f64" => FieldKind::OptionF64,
                                    _ => FieldKind::OptionInt(inner.clone()),
                                };
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
/// 3. **`{Name}Wire::into_domain(self) -> Name`** — converts the wire struct
///    back. This is an `unsafe` method for structs containing enum fields
///    (since the enum discriminant is not validated), or a safe `From` impl
///    for structs without enum fields.
///
/// # Field type mappings
///
/// | Source type | Wire type | To wire | From wire |
/// |---|---|---|---|
/// | `f32`, `f64`, `u8`..`u128`, `i8`..`i128` | same | passthrough | passthrough |
/// | `usize` | `u64` | `as u64` | `as usize` |
/// | `isize` | `i64` | `as i64` | `as isize` |
/// | `bool` | `u8` | `if v { 1 } else { 0 }` | `v != 0` |
/// | `Option<T>` (T integer) | `X_value: u64, X_has: u8` | `Some(v) => (v as u64, 1), None => (0, 0)` | `has != 0 => Some(value as T), else None` |
/// | `Option<f32>` | `X_value: u64, X_has: u8` | `Some(v) => (v.to_bits() as u64, 1), None => (0, 0)` | `has != 0 => Some(f32::from_bits(value as u32)), else None` |
/// | `Option<f64>` | `X_value: u64, X_has: u8` | `Some(v) => (v.to_bits(), 1), None => (0, 0)` | `has != 0 => Some(f64::from_bits(value)), else None` |
/// | `[T; N]` (T: Pod) | same | passthrough | passthrough |
/// | Any other type | `u8` | `v as u8` | `transmute(v)` (unsafe) |
///
/// # Enum safety
///
/// Enum fields are stored as raw `u8` on the wire. Converting back requires
/// that the byte holds a valid discriminant. Because the macro cannot verify
/// enum variants at compile time, structs with enum fields generate an
/// `unsafe fn into_domain(self) -> DomainType` method on the wire struct
/// instead of a safe `From` impl. Callers must ensure enum fields contain
/// valid discriminants (which is always the case when the wire data was
/// produced by a valid domain value via `From<Domain> for Wire`).
///
/// The enum **must** have `#[repr(u8)]` — the macro emits a compile-time
/// `size_of` check to enforce this.
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
/// // Generates: OrderWire, From<Order> for OrderWire,
/// //            OrderWire::into_domain (unsafe, due to enum field)
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
    let mut has_enum_fields = false;

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
            FieldKind::OptionInt(inner) => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u64 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v as u64,
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(src.#value_field as #inner)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionF32 => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u64 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v.to_bits() as u64,
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(f32::from_bits(src.#value_field as u32))
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionF64 => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u64 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v.to_bits(),
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(f64::from_bits(src.#value_field))
                    } else {
                        None
                    }
                });
            }
            FieldKind::Enum => {
                has_enum_fields = true;
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

    // If the struct has enum fields, generate an unsafe `into_domain` method
    // instead of a safe `From` impl to avoid exposing transmute through safe code.
    let from_wire_impl = if has_enum_fields {
        quote! {
            impl #wire_name {
                /// Convert wire struct back to domain struct.
                ///
                /// # Safety
                ///
                /// Enum fields must contain valid discriminant values.
                /// This is guaranteed when the wire data was produced by
                /// `From<#name> for #wire_name`.
                #[inline]
                pub unsafe fn into_domain(self) -> #name {
                    let src = self;
                    #name {
                        #(#from_wire),*
                    }
                }
            }
        }
    } else {
        quote! {
            impl From<#wire_name> for #name {
                #[inline]
                fn from(src: #wire_name) -> Self {
                    #name {
                        #(#from_wire),*
                    }
                }
            }
        }
    };

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

        #from_wire_impl
    };

    TokenStream::from(expanded)
}
