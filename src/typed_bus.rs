// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use alloc::boxed::Box;
use alloc::string::String;
use core::any::{Any, TypeId};
use hashbrown::HashMap;
use spin::Mutex;

use crate::channel::{self, Publisher, Subscribable, Subscriber};
use crate::pod::Pod;

/// Wrapper that erases the concrete `T` behind `dyn Any` while preserving
/// the `TypeId` and a human-readable type name for diagnostics.
struct TopicSlot {
    type_id: TypeId,
    type_name: &'static str,
    inner: Box<dyn Any + Send + Sync>,
}

/// The concrete (generic) payload stored inside a [`TopicSlot`].
struct TypedEntry<T: Pod> {
    subscribable: Subscribable<T>,
    publisher: Option<Publisher<T>>,
}

// Safety: `TypedEntry<T>` is `Send + Sync` when `T: Pod`
// because `Subscribable<T>` is `Send + Sync` and `Publisher<T>` is `Send`.
// The `Option` wrapper and the fact that the publisher is only accessed
// under the outer `Mutex` make this safe.
unsafe impl<T: Pod> Send for TypedEntry<T> {}
unsafe impl<T: Pod> Sync for TypedEntry<T> {}

/// A topic bus that supports different message types per topic.
///
/// Unlike [`Photon<T>`](crate::Photon) which requires a single message type
/// across all topics, `TypedBus` allows each topic to have its own
/// `T: Pod`.
///
/// # Example
///
/// ```
/// use photon_ring::TypedBus;
///
/// let bus = TypedBus::new(1024);
///
/// // Different types per topic
/// let mut price_pub = bus.publisher::<f64>("prices");
/// let mut vol_pub = bus.publisher::<u32>("volumes");
///
/// let mut price_sub = bus.subscribe::<f64>("prices");
/// let mut vol_sub = bus.subscribe::<u32>("volumes");
///
/// price_pub.publish(42.5);
/// vol_pub.publish(1000);
///
/// assert_eq!(price_sub.try_recv(), Ok(42.5));
/// assert_eq!(vol_sub.try_recv(), Ok(1000));
/// ```
pub struct TypedBus {
    topics: Mutex<HashMap<String, TopicSlot>>,
    default_capacity: usize,
}

impl TypedBus {
    /// Create a bus. `capacity` is the ring size for each topic (power of two).
    pub fn new(capacity: usize) -> Self {
        TypedBus {
            topics: Mutex::new(HashMap::new()),
            default_capacity: capacity,
        }
    }

    /// Take the publisher for a topic. Creates the topic if it doesn't exist.
    ///
    /// # Panics
    ///
    /// - Panics if the topic already exists with a different type `T`.
    /// - Panics if the publisher for this topic was already taken.
    pub fn publisher<T: Pod>(&self, topic: &str) -> Publisher<T> {
        let mut topics = self.topics.lock();
        let slot = topics
            .entry_ref(topic)
            .or_insert_with(|| Self::make_slot::<T>(self.default_capacity));
        let entry = Self::downcast_mut::<T>(slot, topic);
        entry
            .publisher
            .take()
            .unwrap_or_else(|| panic!("publisher already taken for topic '{topic}'"))
    }

    /// Try to take the publisher for a topic. Returns `None` if the
    /// publisher was already taken.
    ///
    /// # Panics
    ///
    /// Panics if the topic already exists with a different type `T`.
    pub fn try_publisher<T: Pod>(&self, topic: &str) -> Option<Publisher<T>> {
        let mut topics = self.topics.lock();
        let slot = topics
            .entry_ref(topic)
            .or_insert_with(|| Self::make_slot::<T>(self.default_capacity));
        let entry = Self::downcast_mut::<T>(slot, topic);
        entry.publisher.take()
    }

    /// Subscribe to a topic (future messages only). Creates the topic if needed.
    ///
    /// # Panics
    ///
    /// Panics if the topic already exists with a different type `T`.
    pub fn subscribe<T: Pod>(&self, topic: &str) -> Subscriber<T> {
        let mut topics = self.topics.lock();
        let slot = topics
            .entry_ref(topic)
            .or_insert_with(|| Self::make_slot::<T>(self.default_capacity));
        let entry = Self::downcast_mut::<T>(slot, topic);
        entry.subscribable.subscribe()
    }

    /// Get the clone-able subscriber factory for a topic.
    ///
    /// # Panics
    ///
    /// Panics if the topic already exists with a different type `T`.
    pub fn subscribable<T: Pod>(&self, topic: &str) -> Subscribable<T> {
        let mut topics = self.topics.lock();
        let slot = topics
            .entry_ref(topic)
            .or_insert_with(|| Self::make_slot::<T>(self.default_capacity));
        let entry = Self::downcast_mut::<T>(slot, topic);
        entry.subscribable.clone()
    }

    /// Downcast the erased `TopicSlot` to `TypedEntry<T>`, panicking with a
    /// clear message on type mismatch.
    fn downcast_mut<'a, T: Pod>(slot: &'a mut TopicSlot, topic: &str) -> &'a mut TypedEntry<T> {
        let requested = TypeId::of::<T>();
        if slot.type_id != requested {
            panic!(
                "topic '{topic}' exists with type '{}', cannot access as '{}'",
                slot.type_name,
                core::any::type_name::<T>(),
            );
        }
        slot.inner
            .downcast_mut::<TypedEntry<T>>()
            .expect("TypeId matched but downcast failed (this is a bug)")
    }

    fn make_slot<T: Pod>(capacity: usize) -> TopicSlot {
        let (pub_, sub_) = channel::channel::<T>(capacity);
        TopicSlot {
            type_id: TypeId::of::<T>(),
            type_name: core::any::type_name::<T>(),
            inner: Box::new(TypedEntry {
                subscribable: sub_,
                publisher: Some(pub_),
            }),
        }
    }
}
