//! Behavioral contracts shared by native deadline waits and the portable fallback.
use super::DeadlineWaiter;
use std::future::{poll_fn, Future};
use std::task::Poll;
use std::time::{Duration, Instant};

fn waiters() -> Vec<DeadlineWaiter> {
    let native = DeadlineWaiter::new();
    assert!(
        native.native.is_some(),
        "native backend must initialize in timer tests"
    );
    vec![native, DeadlineWaiter { native: None }]
}

#[tokio::test]
async fn deadline_wait_completes_expired_and_rearmed_deadlines_without_early_return() {
    for mut waiter in waiters() {
        let started_native = waiter.native.is_some();
        waiter
            .wait(Some(Instant::now() - Duration::from_millis(1)))
            .await;
        for millis in [20, 1, 30, 2, 10] {
            let deadline = Instant::now() + Duration::from_millis(millis);
            waiter.wait(Some(deadline)).await;
            assert!(
                Instant::now() >= deadline,
                "deadline completed before eligibility"
            );
        }
        assert_eq!(
            waiter.native.is_some(),
            started_native,
            "backend unexpectedly failed"
        );
    }
}

#[tokio::test]
async fn deadline_wait_survives_repeated_select_cancellation() {
    for mut waiter in waiters() {
        let started_native = waiter.native.is_some();
        for _ in 0..1000 {
            let wait = waiter.wait(Some(Instant::now() + Duration::from_secs(60)));
            tokio::pin!(wait);
            poll_fn(|cx| {
                assert!(wait.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        let deadline = Instant::now() + Duration::from_millis(2);
        waiter.wait(Some(deadline)).await;
        assert!(Instant::now() >= deadline);
        assert_eq!(
            waiter.native.is_some(),
            started_native,
            "backend unexpectedly failed"
        );
    }
}

#[tokio::test]
async fn deadline_wait_discards_expired_cancelled_readiness() {
    for mut waiter in waiters() {
        let started_native = waiter.native.is_some();
        for _ in 0..10 {
            {
                let wait = waiter.wait(Some(Instant::now() + Duration::from_millis(50)));
                tokio::pin!(wait);
                poll_fn(|cx| {
                    assert!(wait.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                // Let the kernel/runtime timer expire without consuming its wait.
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            let deadline = Instant::now() + Duration::from_millis(3);
            waiter.wait(Some(deadline)).await;
            assert!(
                Instant::now() >= deadline,
                "cancelled expiration completed a later wait"
            );
        }
        assert_eq!(
            waiter.native.is_some(),
            started_native,
            "backend unexpectedly failed"
        );
    }
}

#[tokio::test]
async fn deadline_wait_without_deadline_stays_pending_after_activity() {
    for mut waiter in waiters() {
        let started_native = waiter.native.is_some();
        waiter
            .wait(Some(Instant::now() + Duration::from_millis(1)))
            .await;
        {
            let wait = waiter.wait(None);
            tokio::pin!(wait);
            tokio::select! {
                _ = &mut wait => panic!("absent deadline completed"),
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
        }
        assert_eq!(
            waiter.native.is_some(),
            started_native,
            "backend unexpectedly failed"
        );
    }
}
