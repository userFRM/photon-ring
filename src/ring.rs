use crate::slot::Slot;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;

/// Cache-line padding to prevent false sharing between hot atomics.
#[repr(align(64))]
pub(crate) struct Padded<T>(pub(crate) T);

/// Shared ring buffer: a pre-allocated array of seqlock-stamped slots
/// plus the producer cursor.
///
/// The cursor stores the sequence number of the last published message
/// (`u64::MAX` means nothing published yet).
pub(crate) struct SharedRing<T> {
    slots: Box<[Slot<T>]>,
    pub(crate) mask: u64,
    pub(crate) cursor: Padded<AtomicU64>,
}

impl<T: Copy> SharedRing<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );
        assert!(capacity >= 2, "capacity must be at least 2");

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            mask: (capacity - 1) as u64,
            cursor: Padded(AtomicU64::new(u64::MAX)),
        }
    }

    #[inline]
    pub(crate) fn slot(&self, seq: u64) -> &Slot<T> {
        unsafe { self.slots.get_unchecked((seq & self.mask) as usize) }
    }

    #[inline]
    pub(crate) fn capacity(&self) -> u64 {
        self.mask + 1
    }
}
