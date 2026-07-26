#![cfg(feature = "atomic-slots")]
//! Provenance regression: the striped atomic path must not access beyond
//! `size_of::<T>()`, for payload sizes that are not a multiple of 8.
use photon_ring::channel;

macro_rules! roundtrip {
    ($name:ident, $t:ty, $v:expr) => {
        #[test]
        fn $name() {
            let (mut p, s) = channel::<$t>(8);
            let mut sub = s.subscribe();
            p.publish($v);
            assert_eq!(sub.try_recv(), Ok($v));
        }
    };
}

roundtrip!(sz1_u8, u8, 0xABu8);
roundtrip!(sz2_u16, u16, 0xBEEFu16);
roundtrip!(sz4_u32, u32, 0xDEAD_BEEFu32);
roundtrip!(sz8_u64, u64, 0x0123_4567_89AB_CDEFu64);
roundtrip!(
    sz16_u128,
    u128,
    0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEFu128
);
// Sizes 3,5,6,7,12,15 exercise every tail decomposition path.
roundtrip!(sz3, [u8; 3], [1, 2, 3]);
roundtrip!(sz5, [u8; 5], [1, 2, 3, 4, 5]);
roundtrip!(sz6, [u8; 6], [1, 2, 3, 4, 5, 6]);
roundtrip!(sz7, [u8; 7], [1, 2, 3, 4, 5, 6, 7]);
roundtrip!(sz12, [u32; 3], [7, 8, 9]);
roundtrip!(sz15, [u8; 15], [9; 15]);
