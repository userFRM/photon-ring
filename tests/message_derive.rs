// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tests for the `Message` derive macro.
//!
//! Run with: `cargo test --features derive --test message_derive`

#![cfg(feature = "derive")]

// -------------------------------------------------------------------------
// Setup: types used in tests
// -------------------------------------------------------------------------

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Buy = 0,
    Sell = 1,
}

#[derive(photon_ring::DeriveMessage)]
struct Order {
    price: f64,
    qty: u32,
    side: Side,
    filled: bool,
    tag: Option<u32>,
}

// -------------------------------------------------------------------------
// Wire struct exists and is Pod
// -------------------------------------------------------------------------

#[test]
fn wire_struct_is_pod() {
    fn assert_pod<T: photon_ring::Pod>() {}
    assert_pod::<OrderWire>();
}

#[test]
fn wire_struct_is_copy() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<OrderWire>();
}

// -------------------------------------------------------------------------
// Round-trip conversions
// -------------------------------------------------------------------------

#[test]
fn order_to_wire_and_back() {
    let order = Order {
        price: 123.45,
        qty: 100,
        side: Side::Buy,
        filled: true,
        tag: Some(42),
    };

    let wire: OrderWire = order.into();
    assert_eq!(wire.price, 123.45);
    assert_eq!(wire.qty, 100);
    assert_eq!(wire.side, 0); // Buy = 0
    assert_eq!(wire.filled, 1); // true = 1
    assert_eq!(wire.tag_value, 42); // Some(42) value
    assert_eq!(wire.tag_has, 1); // Some(_) present

    // Enum fields use unsafe into_domain
    let back: Order = unsafe { wire.into_domain() };
    assert_eq!(back.price, 123.45);
    assert_eq!(back.qty, 100);
    assert_eq!(back.side, Side::Buy);
    assert!(back.filled);
    assert_eq!(back.tag, Some(42));
}

#[test]
fn bool_false_roundtrip() {
    let order = Order {
        price: 0.0,
        qty: 0,
        side: Side::Sell,
        filled: false,
        tag: None,
    };

    let wire: OrderWire = order.into();
    assert_eq!(wire.filled, 0);
    assert_eq!(wire.side, 1); // Sell = 1
    assert_eq!(wire.tag_has, 0); // None

    let back: Order = unsafe { wire.into_domain() };
    assert!(!back.filled);
    assert_eq!(back.side, Side::Sell);
    assert_eq!(back.tag, None);
}

#[test]
fn option_none_is_zero() {
    let order = Order {
        price: 1.0,
        qty: 1,
        side: Side::Buy,
        filled: false,
        tag: None,
    };

    let wire: OrderWire = order.into();
    assert_eq!(wire.tag_has, 0);

    let back: Order = unsafe { wire.into_domain() };
    assert_eq!(back.tag, None);
}

#[test]
fn option_some_zero_roundtrip() {
    // CRITICAL 1 regression test: Some(0) must NOT be confused with None.
    let order = Order {
        price: 1.0,
        qty: 1,
        side: Side::Buy,
        filled: false,
        tag: Some(0),
    };

    let wire: OrderWire = order.into();
    assert_eq!(wire.tag_value, 0);
    assert_eq!(wire.tag_has, 1); // has flag distinguishes Some(0) from None

    let back: Order = unsafe { wire.into_domain() };
    assert_eq!(back.tag, Some(0));
}

// -------------------------------------------------------------------------
// Publish wire struct through a Photon Ring channel
// -------------------------------------------------------------------------

#[test]
fn wire_struct_through_channel() {
    let (mut pub_, subs) = photon_ring::channel::<OrderWire>(4);
    let mut sub = subs.subscribe();

    let order = Order {
        price: 99.99,
        qty: 50,
        side: Side::Sell,
        filled: true,
        tag: Some(7),
    };

    pub_.publish(order.into());
    let wire = sub.try_recv().unwrap();
    let back: Order = unsafe { wire.into_domain() };

    assert_eq!(back.price, 99.99);
    assert_eq!(back.qty, 50);
    assert_eq!(back.side, Side::Sell);
    assert!(back.filled);
    assert_eq!(back.tag, Some(7));
}

// -------------------------------------------------------------------------
// All-numeric struct (passthrough only) -- no enum, so From is generated
// -------------------------------------------------------------------------

#[derive(photon_ring::DeriveMessage)]
struct Tick {
    bid: f64,
    ask: f64,
    volume: u64,
}

#[test]
fn all_numeric_passthrough() {
    let tick = Tick {
        bid: 1.23,
        ask: 4.56,
        volume: 1000,
    };

    let wire: TickWire = tick.into();
    assert_eq!(wire.bid, 1.23);
    assert_eq!(wire.ask, 4.56);
    assert_eq!(wire.volume, 1000);

    // No enum fields, so safe From is generated
    let back: Tick = wire.into();
    assert_eq!(back.bid, 1.23);
    assert_eq!(back.ask, 4.56);
    assert_eq!(back.volume, 1000);
}

// -------------------------------------------------------------------------
// usize / isize fields
// -------------------------------------------------------------------------

#[derive(photon_ring::DeriveMessage)]
struct Indexed {
    index: usize,
    offset: isize,
    value: u32,
}

#[test]
fn usize_isize_conversion() {
    let src = Indexed {
        index: 42,
        offset: -7,
        value: 100,
    };

    let wire: IndexedWire = src.into();
    assert_eq!(wire.index, 42u64);
    assert_eq!(wire.offset, -7i64);
    assert_eq!(wire.value, 100);

    // No enum fields, so safe From is generated
    let back: Indexed = wire.into();
    assert_eq!(back.index, 42usize);
    assert_eq!(back.offset, -7isize);
    assert_eq!(back.value, 100);
}

// -------------------------------------------------------------------------
// Array fields
// -------------------------------------------------------------------------

#[derive(photon_ring::DeriveMessage)]
struct WithArray {
    data: [u8; 4],
    value: u32,
}

#[test]
fn array_passthrough() {
    let src = WithArray {
        data: [1, 2, 3, 4],
        value: 99,
    };

    let wire: WithArrayWire = src.into();
    assert_eq!(wire.data, [1, 2, 3, 4]);
    assert_eq!(wire.value, 99);

    // No enum fields, so safe From is generated
    let back: WithArray = wire.into();
    assert_eq!(back.data, [1, 2, 3, 4]);
    assert_eq!(back.value, 99);
}

// -------------------------------------------------------------------------
// Option<f32> and Option<f64> precision preservation
// -------------------------------------------------------------------------

#[derive(photon_ring::DeriveMessage)]
struct FloatOptions {
    opt_f32: Option<f32>,
    opt_f64: Option<f64>,
    plain: u32,
}

#[test]
fn option_f64_preserves_precision() {
    let src = FloatOptions {
        opt_f32: Some(1.5f32),
        opt_f64: Some(1.5f64),
        plain: 1,
    };

    let wire: FloatOptionsWire = src.into();
    assert_eq!(wire.opt_f32_has, 1);
    assert_eq!(wire.opt_f64_has, 1);

    // No enum fields, so safe From is generated
    let back: FloatOptions = wire.into();
    assert_eq!(back.opt_f32, Some(1.5f32));
    assert_eq!(back.opt_f64, Some(1.5f64));
}

#[test]
fn option_f64_none_roundtrip() {
    let src = FloatOptions {
        opt_f32: None,
        opt_f64: None,
        plain: 42,
    };

    let wire: FloatOptionsWire = src.into();
    assert_eq!(wire.opt_f32_has, 0);
    assert_eq!(wire.opt_f64_has, 0);

    let back: FloatOptions = wire.into();
    assert_eq!(back.opt_f32, None);
    assert_eq!(back.opt_f64, None);
    assert_eq!(back.plain, 42);
}

#[test]
fn option_f64_zero_value_roundtrip() {
    // Some(0.0) must NOT be confused with None
    let src = FloatOptions {
        opt_f32: Some(0.0f32),
        opt_f64: Some(0.0f64),
        plain: 0,
    };

    let wire: FloatOptionsWire = src.into();
    assert_eq!(wire.opt_f32_has, 1);
    assert_eq!(wire.opt_f64_has, 1);

    let back: FloatOptions = wire.into();
    assert_eq!(back.opt_f32, Some(0.0f32));
    assert_eq!(back.opt_f64, Some(0.0f64));
}

#[test]
fn option_f64_fractional_preserved() {
    // Fractional values must not be truncated to integers
    let src = FloatOptions {
        opt_f32: Some(3.125f32),
        opt_f64: Some(2.719f64),
        plain: 0,
    };

    let wire: FloatOptionsWire = src.into();
    let back: FloatOptions = wire.into();
    assert_eq!(back.opt_f32, Some(3.125f32));
    assert_eq!(back.opt_f64, Some(2.719f64));
}

// -------------------------------------------------------------------------
// Option<u128>, Option<i128>, Option<usize>, Option<isize> regression tests
// -------------------------------------------------------------------------

#[derive(photon_ring::DeriveMessage)]
struct WideOptions {
    a: u64,
    big_u: Option<u128>,
    big_i: Option<i128>,
    sz: Option<usize>,
    isz: Option<isize>,
}

#[test]
fn option_u128_max_roundtrip() {
    let src = WideOptions {
        a: 1,
        big_u: Some(u128::MAX),
        big_i: Some(-1i128),
        sz: Some(usize::MAX),
        isz: Some(-1isize),
    };
    let wire: WideOptionsWire = src.into();
    assert_eq!(wire.big_u_has, 1);
    assert_eq!(wire.big_u_value, u128::MAX);
    let back: WideOptions = wire.into();
    assert_eq!(back.big_u, Some(u128::MAX));
    assert_eq!(back.big_i, Some(-1i128));
    assert_eq!(back.sz, Some(usize::MAX));
    assert_eq!(back.isz, Some(-1isize));
}

#[test]
fn option_u128_none_roundtrip() {
    let src = WideOptions {
        a: 0,
        big_u: None,
        big_i: None,
        sz: None,
        isz: None,
    };
    let wire: WideOptionsWire = src.into();
    assert_eq!(wire.big_u_has, 0);
    assert_eq!(wire.big_i_has, 0);
    assert_eq!(wire.sz_has, 0);
    assert_eq!(wire.isz_has, 0);
    let back: WideOptions = wire.into();
    assert_eq!(back.big_u, None);
    assert_eq!(back.big_i, None);
    assert_eq!(back.sz, None);
    assert_eq!(back.isz, None);
}

#[test]
fn option_u128_zero_roundtrip() {
    let src = WideOptions {
        a: 0,
        big_u: Some(0u128),
        big_i: Some(0i128),
        sz: Some(0usize),
        isz: Some(0isize),
    };
    let wire: WideOptionsWire = src.into();
    assert_eq!(wire.big_u_has, 1);
    assert_eq!(wire.big_u_value, 0);
    let back: WideOptions = wire.into();
    assert_eq!(back.big_u, Some(0u128));
    assert_eq!(back.big_i, Some(0i128));
    assert_eq!(back.sz, Some(0usize));
    assert_eq!(back.isz, Some(0isize));
}
