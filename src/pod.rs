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
