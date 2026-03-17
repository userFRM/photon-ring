// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use alloc::string::{String, ToString};

use crate::channel::{self, Publisher, Subscribable, Subscriber};
use crate::pod::Pod;
use hashbrown::HashMap;
use spin::Mutex;

/// Named-topic pub/sub bus.
///
/// Wraps [`channel`](crate::channel) with string-keyed topic routing.
/// Each topic is an independent SPMC ring.
///
/// ```
/// let bus = photon_ring::Photon::<u64>::new(64);
/// let mut pub_ = bus.publisher("prices");
/// let mut sub  = bus.subscribe("prices");
/// pub_.publish(100);
/// assert_eq!(sub.try_recv(), Ok(100));
/// ```
pub struct Photon<T: Pod> {
    topics: Mutex<HashMap<String, TopicEntry<T>>>,
    default_capacity: usize,
}

struct TopicEntry<T: Pod> {
    subscribable: Subscribable<T>,
    publisher: Option<Publisher<T>>,
}

impl<T: Pod> Photon<T> {
    /// Create a bus. `capacity` is the ring size for each topic (power of two).
    pub fn new(capacity: usize) -> Self {
        Photon {
            topics: Mutex::new(HashMap::new()),
            default_capacity: capacity,
        }
    }

    /// Take the publisher for a topic. Creates the topic if it doesn't exist.
    ///
    /// # Panics
    /// Panics if the publisher for this topic was already taken.
    pub fn publisher(&self, topic: &str) -> Publisher<T> {
        let mut topics = self.topics.lock();
        if !topics.contains_key(topic) {
            topics.insert(topic.to_string(), Self::make_entry(self.default_capacity));
        }
        let entry = topics.get_mut(topic).unwrap();
        entry
            .publisher
            .take()
            .unwrap_or_else(|| panic!("publisher already taken for topic '{}'", topic))
    }

    /// Try to take the publisher for a topic. Returns `None` if the
    /// publisher was already taken.
    pub fn try_publisher(&self, topic: &str) -> Option<Publisher<T>> {
        let mut topics = self.topics.lock();
        if !topics.contains_key(topic) {
            topics.insert(topic.to_string(), Self::make_entry(self.default_capacity));
        }
        let entry = topics.get_mut(topic).unwrap();
        entry.publisher.take()
    }

    /// Subscribe to a topic (future messages only). Creates the topic if needed.
    pub fn subscribe(&self, topic: &str) -> Subscriber<T> {
        let mut topics = self.topics.lock();
        if !topics.contains_key(topic) {
            topics.insert(topic.to_string(), Self::make_entry(self.default_capacity));
        }
        let entry = topics.get_mut(topic).unwrap();
        entry.subscribable.subscribe()
    }

    /// Get the clone-able subscriber factory for a topic.
    pub fn subscribable(&self, topic: &str) -> Subscribable<T> {
        let mut topics = self.topics.lock();
        if !topics.contains_key(topic) {
            topics.insert(topic.to_string(), Self::make_entry(self.default_capacity));
        }
        let entry = topics.get_mut(topic).unwrap();
        entry.subscribable.clone()
    }

    fn make_entry(capacity: usize) -> TopicEntry<T> {
        let (pub_, sub_) = channel::channel(capacity);
        TopicEntry {
            subscribable: sub_,
            publisher: Some(pub_),
        }
    }
}
