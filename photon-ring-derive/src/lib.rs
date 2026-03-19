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
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, GenericArgument, Meta, PathArguments, Type,
};

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

    // Verify #[repr(C)] is present
    let has_repr_c = input.attrs.iter().any(|attr| {
        if !attr.path().is_ident("repr") {
            return false;
        }
        let mut found = false;
        if let Meta::List(list) = &attr.meta {
            let _ = list.parse_nested_meta(|nested| {
                if nested.path.is_ident("C") {
                    found = true;
                }
                Ok(())
            });
        }
        found
    });
    if !has_repr_c {
        return syn::Error::new_spanned(
            &input.ident,
            "Pod can only be derived for #[repr(C)] structs",
        )
        .to_compile_error()
        .into();
    }

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
    /// `Option<T>` where T is an unsigned ≤64-bit integer type (u8, u16, u32, u64).
    /// Wire struct gets two fields: `X_value: u64` and `X_has: u8`.
    /// Stores the inner type for the back-conversion cast.
    OptionUnsignedInt(Type),
    /// `Option<T>` where T is a signed ≤64-bit integer type (i8, i16, i32, i64).
    /// Wire struct gets two fields: `X_value: i64` and `X_has: u8`.
    /// Stores the inner type for the back-conversion cast.
    OptionSignedInt(Type),
    /// `Option<bool>` — wire struct gets `X_value: u8` and `X_has: u8`.
    OptionBool,
    /// `Option<u128>` — wire struct gets `X_value: u128` and `X_has: u8`.
    OptionU128,
    /// `Option<i128>` — wire struct gets `X_value: u128` and `X_has: u8`.
    OptionI128,
    /// `Option<usize>` — wire struct gets `X_value: u64` and `X_has: u8`.
    OptionUsize,
    /// `Option<isize>` — wire struct gets `X_value: i64` and `X_has: u8`.
    OptionIsize,
    /// `Option<f32>` — wire struct gets `X_value: u32` (bit-encoded) and `X_has: u8`.
    OptionF32,
    /// `Option<f64>` — wire struct gets `X_value: u64` (bit-encoded) and `X_has: u8`.
    OptionF64,
    /// Any other type — assumed to be a `#[repr(u8)]` enum → `u8`.
    Enum,
    /// Unsupported `Option<T>` inner type — will emit a compile error.
    UnsupportedOption(String),
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
                            let name = type_name(inner).unwrap_or_default();
                            return match name.as_str() {
                                "bool" => FieldKind::OptionBool,
                                "f32" => FieldKind::OptionF32,
                                "f64" => FieldKind::OptionF64,
                                "u128" => FieldKind::OptionU128,
                                "i128" => FieldKind::OptionI128,
                                "usize" => FieldKind::OptionUsize,
                                "isize" => FieldKind::OptionIsize,
                                "u8" | "u16" | "u32" | "u64" => {
                                    FieldKind::OptionUnsignedInt(inner.clone())
                                }
                                "i8" | "i16" | "i32" | "i64" => {
                                    FieldKind::OptionSignedInt(inner.clone())
                                }
                                _ => FieldKind::UnsupportedOption(name),
                            };
                        }
                    }
                    FieldKind::UnsupportedOption(String::new())
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
/// | `Option<T>` (T: unsigned ≤64-bit) | `X_value: u64, X_has: u8` | `Some(v) => (v as u64, 1), None => (0, 0)` | `has != 0 => Some(value as T), else None` |
/// | `Option<T>` (T: signed ≤64-bit) | `X_value: i64, X_has: u8` | `Some(v) => (v as i64, 1), None => (0, 0)` | `has != 0 => Some(value as T), else None` |
/// | `Option<u128>` | `X_value: u128, X_has: u8` | `Some(v) => (v, 1), None => (0, 0)` | `has != 0 => Some(value), else None` |
/// | `Option<i128>` | `X_value: u128, X_has: u8` | `Some(v) => (v as u128, 1), None => (0, 0)` | `has != 0 => Some(value as i128), else None` |
/// | `Option<usize>` | `X_value: u64, X_has: u8` | `Some(v) => (v as u64, 1), None => (0, 0)` | `has != 0 => Some(value as usize), else None` |
/// | `Option<isize>` | `X_value: i64, X_has: u8` | `Some(v) => (v as i64, 1), None => (0, 0)` | `has != 0 => Some(value as isize), else None` |
/// | `Option<f32>` | `X_value: u32, X_has: u8` | `Some(v) => (v.to_bits(), 1), None => (0, 0)` | `has != 0 => Some(f32::from_bits(value)), else None` |
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
    let mut has_usize_isize = false;

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
                has_usize_isize = true;
                wire_fields.push(quote! { pub #fname: u64 });
                to_wire.push(quote! { #fname: src.#fname as u64 });
                from_wire.push(quote! { #fname: src.#fname as usize });
            }
            FieldKind::Isize => {
                has_usize_isize = true;
                wire_fields.push(quote! { pub #fname: i64 });
                to_wire.push(quote! { #fname: src.#fname as i64 });
                from_wire.push(quote! { #fname: src.#fname as isize });
            }
            FieldKind::OptionUnsignedInt(inner) => {
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
            FieldKind::OptionSignedInt(inner) => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: i64 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v as i64,
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
            FieldKind::OptionBool => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u8 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => if v { 1 } else { 0 },
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(src.#value_field != 0)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionU128 => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u128 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v,
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(src.#value_field)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionI128 => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u128 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v as u128,
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(src.#value_field as i128)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionUsize => {
                has_usize_isize = true;
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
                        Some(src.#value_field as usize)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionIsize => {
                has_usize_isize = true;
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: i64 });
                wire_fields.push(quote! { pub #has_field: u8 });
                to_wire.push(quote! {
                    #value_field: match src.#fname {
                        Some(v) => v as i64,
                        None => 0,
                    }
                });
                to_wire.push(quote! {
                    #has_field: if src.#fname.is_some() { 1 } else { 0 }
                });
                from_wire.push(quote! {
                    #fname: if src.#has_field != 0 {
                        Some(src.#value_field as isize)
                    } else {
                        None
                    }
                });
            }
            FieldKind::OptionF32 => {
                let value_field = format_ident!("{}_value", fname);
                let has_field = format_ident!("{}_has", fname);
                wire_fields.push(quote! { pub #value_field: u32 });
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
                        Some(f32::from_bits(src.#value_field))
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
                    // SAFETY: This transmute converts a raw u8 back to the enum type.
                    // This is sound ONLY when the byte contains a valid discriminant.
                    // The wire struct should only be constructed via `From<DomainType>`,
                    // which guarantees valid discriminants. Constructing the wire struct
                    // from arbitrary bytes and calling `into_domain()` is undefined
                    // behavior if any enum field holds an invalid discriminant.
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
            FieldKind::UnsupportedOption(inner_name) => {
                let msg = format!(
                    "Message derive: field `{}` has unsupported type `Option<{}>`. \
                     Only Option<bool>, Option<integer>, Option<f32>, and Option<f64> \
                     are supported.",
                    fname, inner_name,
                );
                return syn::Error::new_spanned(fty, msg).to_compile_error().into();
            }
        }
    }

    // H5: Compile-time assertion that usize/isize fit in u64/i64 (documents
    // the 64-bit assumption and fails loudly on platforms where it does not hold).
    if has_usize_isize {
        assertions.push(quote! {
            const _: () = assert!(
                core::mem::size_of::<usize>() <= core::mem::size_of::<u64>(),
                "photon-ring Message derive requires usize to fit in u64",
            );
        });
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
                /// Enum fields are stored as raw `u8` and converted back via
                /// `core::mem::transmute`. The caller **must** ensure every enum
                /// field contains a valid discriminant value. This is guaranteed
                /// when the wire struct was produced by `From<DomainType>` — but
                /// constructing the wire struct from arbitrary bytes (e.g. reading
                /// raw memory, deserialization) and calling this method is
                /// **undefined behavior** if any enum field holds an invalid
                /// discriminant.
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

    // C3: Add a doc warning on the wire struct when it contains enum fields
    let wire_struct_doc = if has_enum_fields {
        quote! {
            /// Auto-generated Pod-compatible wire struct for the domain type.
            ///
            /// # Warning
            ///
            /// This struct contains enum fields stored as raw `u8`. Constructing
            /// it from arbitrary bytes (not via `From<DomainType>`) and then calling
            /// `into_domain()` can cause **undefined behavior** if any enum field
            /// holds an invalid discriminant value.
        }
    } else {
        quote! {
            /// Auto-generated Pod-compatible wire struct for the domain type.
        }
    };

    let expanded = quote! {
        // Compile-time assertions
        #(#assertions)*

        #wire_struct_doc
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
