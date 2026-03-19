// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use photon_ring_async::{AsyncSubscriber, AsyncSubscriberGroup};

#[test]
fn test_async_recv_single() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut async_sub = AsyncSubscriber::new(subs.subscribe());

        for i in 0..10 {
            pub_.publish(i);
        }

        for i in 0..10 {
            let value = async_sub.recv().await;
            assert_eq!(value, i);
        }
    });
}

#[test]
fn test_async_recv_group() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut group = AsyncSubscriberGroup::<u64, 4>::new(subs.subscribe_group::<4>());

        for i in 0..10 {
            pub_.publish(100 + i);
        }

        for i in 0..10 {
            let value = group.recv().await;
            assert_eq!(value, 100 + i);
        }
    });
}

#[test]
fn test_async_recv_batch() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut async_sub = AsyncSubscriber::new(subs.subscribe());

        for i in 0..10 {
            pub_.publish(i);
        }

        let mut buf = [0u64; 16];
        let count = async_sub.recv_batch(&mut buf).await;
        assert_eq!(count, 10);
        for i in 0..10 {
            assert_eq!(buf[i], i as u64);
        }
    });
}

#[test]
fn test_async_batch_group() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut group = AsyncSubscriberGroup::<u64, 2>::new(subs.subscribe_group::<2>());

        for i in 0..5 {
            pub_.publish(i);
        }

        let mut buf = [0u64; 8];
        let count = group.recv_batch(&mut buf).await;
        assert_eq!(count, 5);
        for i in 0..5 {
            assert_eq!(buf[i], i as u64);
        }
    });
}

#[test]
fn test_async_spin_budget() {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // Create a no-op waker to manually drive polling.
    fn noop_raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);

    let (mut pub_, subs) = photon_ring::channel::<u64>(64);
    let mut async_sub = AsyncSubscriber::with_spin_budget(subs.subscribe(), 4);

    // No messages published — poll_recv should return Pending after 4 tries.
    let result = async_sub.poll_recv(&mut cx);
    assert!(result.is_pending(), "expected Pending with no messages");

    // Publish and try again — should return Ready.
    pub_.publish(42);
    let result = async_sub.poll_recv(&mut cx);
    assert_eq!(result, Poll::Ready(42));
}

#[test]
fn test_async_stats_forwarded() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut async_sub = AsyncSubscriber::new(subs.subscribe());

        assert_eq!(async_sub.total_received(), 0);
        assert_eq!(async_sub.total_lagged(), 0);

        pub_.publish(1);
        pub_.publish(2);
        let _ = async_sub.recv().await;
        let _ = async_sub.recv().await;

        assert_eq!(async_sub.total_received(), 2);
    });
}

#[test]
fn test_async_into_inner() {
    let (_pub_, subs) = photon_ring::channel::<u64>(64);
    let async_sub = AsyncSubscriber::new(subs.subscribe());
    assert_eq!(async_sub.spin_budget(), 64);
    let _inner = async_sub.into_inner();
}

#[test]
fn test_recv_future() {
    pollster::block_on(async {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut async_sub = AsyncSubscriber::new(subs.subscribe());
        pub_.publish(99);

        let fut = photon_ring_async::RecvFuture::new(&mut async_sub);
        let value = fut.await;
        assert_eq!(value, 99);
    });
}
