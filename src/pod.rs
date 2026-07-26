// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The [`Pod`] marker trait for seqlock-safe payload types.

/// Marker trait for types safe to use with seqlock-stamped ring buffers.
///
/// A type is `Pod` ("Plain Old Data") if **every possible bit pattern**
/// of `size_of::<T>()` bytes represents a valid value of `T`. This is
/// stricter than [`Copy`] — it excludes types where certain bit patterns
/// are undefined behavior, such as `bool` (only 0/1 valid), `char`
/// (must be a valid Unicode scalar), `NonZero*` (must be nonzero), and
/// references (must point to valid memory).
///
/// # Why this matters
///
/// The seqlock read protocol performs an optimistic non-atomic read that
/// may observe a partially-written ("torn") value. If the torn bit pattern
/// violates a type's validity invariant, this is undefined behavior even
/// though the value is detected and discarded by the stamp check. `Pod`
/// guarantees that no bit pattern is invalid, making torn reads harmless.
///
/// # Safety
///
/// Implementors must ensure:
/// 1. `T` is `Copy` (no destructor, no move semantics).
/// 2. `T` is `Send` (safe to transfer across threads).
/// 3. Every possible bit pattern of `size_of::<T>()` bytes is a valid `T`.
/// 4. `T` has **no padding bytes**. Padding is uninitialized memory, and the
///    `atomic-slots` feature reads the payload as atomic words — reading an
///    uninitialized byte as part of an integer is undefined behaviour even
///    though every *initialized* bit pattern is valid. Add explicit padding
///    fields (`_pad: [u8; 3]`) so the whole value is initialized rather than
///    letting the compiler insert implicit padding.
///
/// This is why multi-field tuples are **not** `Pod`: `(u8, u64)` has
/// `repr(Rust)` layout with 7 implicit padding bytes. Use a `#[repr(C)]`
/// struct with explicit padding fields for multi-field payloads.
///
/// # What types are NOT `Pod`?
///
/// | Type | Why | What to use instead |
/// |---|---|---|
/// | `bool` | Only 0 and 1 are valid | `u8` (0 = false, 1 = true) |
/// | `char` | Must be valid Unicode scalar | `u32` |
/// | `NonZero<u32>` | Zero is invalid | `u32` |
/// | `Option<T>` | Discriminant has invalid patterns | `u8` sentinel (e.g., 255 = None) |
/// | `enum` (Rust) | Only declared variants are valid | `u8` or `u32` with constants |
/// | `&T`, `&str` | Pointer must be valid | Not supported — use value types |
/// | `String`, `Vec` | Heap-allocated, has `Drop` | Fixed `[u8; N]` buffer |
///
/// # Converting real-world types
///
/// A common pattern: your domain model uses enums and `Option`, but the
/// Photon Ring message struct uses plain integers:
///
/// ```rust
/// // Domain type (NOT Pod — has Option and enum)
/// // enum Side { Buy, Sell }
/// // struct Order { price: f64, qty: u32, side: Side, tag: Option<u32> }
///
/// // Photon Ring message (Pod — all fields are plain numerics)
/// #[repr(C)]
/// #[derive(Clone, Copy)]
/// struct OrderMsg {
///     price: f64,
///     qty: u32,
///     side: u8,      // 0 = Buy, 1 = Sell
///     tag: u32,      // 0 = None, nonzero = Some(value)
///     _pad: [u8; 3], // explicit padding for alignment
/// }
/// unsafe impl photon_ring::Pod for OrderMsg {}
///
/// // Convert at the boundary:
/// // let msg = OrderMsg { price: 100.0, qty: 10, side: 0, tag: 0, _pad: [0;3] };
/// // publisher.publish(msg);
/// ```
///
/// # Pre-implemented types
///
/// `Pod` is implemented for all primitive numeric types, arrays of `Pod`
/// types, and the zero- and one-element tuples. Larger tuples are excluded
/// because their layout may include padding.
///
/// For user-defined structs, use `unsafe impl`:
/// ```
/// #[repr(C)]
/// #[derive(Clone, Copy)]
/// struct Quote {
///     price: f64,
///     volume: u32,
///     _pad: u32,
/// }
///
/// // SAFETY: Quote is #[repr(C)], all fields are plain numerics,
/// // and every bit pattern is a valid Quote.
/// unsafe impl photon_ring::Pod for Quote {}
/// ```
pub unsafe trait Pod: Copy + Send + 'static {}

// Primitive numeric types — every bit pattern is valid
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for u128 {}
unsafe impl Pod for i8 {}
unsafe impl Pod for i16 {}
unsafe impl Pod for i32 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for i128 {}
unsafe impl Pod for f32 {}
unsafe impl Pod for f64 {}
unsafe impl Pod for usize {}
unsafe impl Pod for isize {}

// Arrays of Pod types
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

// Tuples of `Pod` types.
//
// Only the zero- and one-element tuples are covered. A multi-field tuple has
// `repr(Rust)` layout, so the compiler may insert padding between fields —
// `(u8, u64)` carries 7 padding bytes. Padding is uninitialized memory, which
// violates requirement 4 and is undefined to read as part of an atomic word
// under the `atomic-slots` feature. Use a `#[repr(C)]` struct with explicit
// padding fields instead.
unsafe impl Pod for () {}
unsafe impl<A: Pod> Pod for (A,) {}
