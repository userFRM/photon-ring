// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

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
/// 4. `T` has no padding bytes that carry validity constraints.
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
/// types, and tuples up to 12 elements of `Pod` types.
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

// Tuples of Pod types (up to 12, matching standard library convention)
unsafe impl Pod for () {}
unsafe impl<A: Pod> Pod for (A,) {}
unsafe impl<A: Pod, B: Pod> Pod for (A, B) {}
unsafe impl<A: Pod, B: Pod, C: Pod> Pod for (A, B, C) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod> Pod for (A, B, C, D) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod> Pod for (A, B, C, D, E) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod> Pod for (A, B, C, D, E, F) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod> Pod for (A, B, C, D, E, F, G) {}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod> Pod
    for (A, B, C, D, E, F, G, H)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod> Pod
    for (A, B, C, D, E, F, G, H, I)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod, J: Pod> Pod
    for (A, B, C, D, E, F, G, H, I, J)
{
}
unsafe impl<A: Pod, B: Pod, C: Pod, D: Pod, E: Pod, F: Pod, G: Pod, H: Pod, I: Pod, J: Pod, K: Pod>
    Pod for (A, B, C, D, E, F, G, H, I, J, K)
{
}
unsafe impl<
        A: Pod,
        B: Pod,
        C: Pod,
        D: Pod,
        E: Pod,
        F: Pod,
        G: Pod,
        H: Pod,
        I: Pod,
        J: Pod,
        K: Pod,
        L: Pod,
    > Pod for (A, B, C, D, E, F, G, H, I, J, K, L)
{
}
