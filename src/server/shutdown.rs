//! Why the server is stopping, recorded where the request arrives and logged
//! once when shutdown begins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::platform::ServerQuitSignal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    Signal(ServerQuitSignal),
    ApiStop { caller: Option<String> },
    HostShutdown,
    Unknown,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(signal) => write!(f, "{signal}"),
            Self::ApiStop {
                caller: Some(caller),
            } => write!(f, "server.stop request from {caller}"),
            Self::ApiStop { caller: None } => f.write_str("server.stop request"),
            Self::HostShutdown => f.write_str("host shutdown"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// The server's stop flag plus the reason recorded by the first stop request.
#[derive(Clone, Default)]
pub(crate) struct ServerStop {
    flag: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<ShutdownReason>>>,
    notify: Arc<Notify>,
}

impl ServerStop {
    pub(crate) fn flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    pub(crate) fn request(&self, reason: ShutdownReason) {
        if let Ok(mut recorded) = self.reason.lock() {
            recorded.get_or_insert(reason);
        }
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// The flag preserves requests before registration and after cancellation.
    pub(crate) async fn wait_requested(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before checking the flag so a request between the
            // check and await cannot leave this waiter asleep.
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn take_reason(&self) -> Option<ShutdownReason> {
        self.reason.lock().ok()?.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_stop_reason_wins() {
        let stop = ServerStop::default();
        assert!(!stop.is_requested());

        stop.request(ShutdownReason::HostShutdown);
        stop.request(ShutdownReason::ApiStop { caller: None });

        assert!(stop.is_requested());
        assert_eq!(stop.take_reason(), Some(ShutdownReason::HostShutdown));
        assert_eq!(stop.take_reason(), None);
    }

    #[test]
    fn api_stop_reason_names_the_caller() {
        let reason = ShutdownReason::ApiStop {
            caller: Some("pid 42 (herdr), parent pid 7 (zsh)".into()),
        };
        assert_eq!(
            reason.to_string(),
            "server.stop request from pid 42 (herdr), parent pid 7 (zsh)"
        );
    }

    #[tokio::test]
    async fn stop_request_before_wait_remains_observable() {
        let stop = ServerStop::default();
        stop.request(ShutdownReason::HostShutdown);
        stop.wait_requested().await;
        // Every later waiter must observe the durable state too.
        stop.wait_requested().await;
        assert_eq!(stop.take_reason(), Some(ShutdownReason::HostShutdown));
    }

    #[tokio::test]
    async fn stop_request_wakes_all_registered_waiters() {
        struct WakeFlag(AtomicBool);
        impl std::task::Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let stop = ServerStop::default();
        let mut waits = [
            Box::pin(stop.wait_requested()),
            Box::pin(stop.wait_requested()),
        ];
        let wakes = [
            Arc::new(WakeFlag(AtomicBool::new(false))),
            Arc::new(WakeFlag(AtomicBool::new(false))),
        ];
        for (waiting, wake) in waits.iter_mut().zip(&wakes) {
            let waker = std::task::Waker::from(wake.clone());
            let mut context = std::task::Context::from_waker(&waker);
            assert!(std::future::Future::poll(waiting.as_mut(), &mut context).is_pending());
        }
        stop.request(ShutdownReason::ApiStop { caller: None });
        for (waiting, wake) in waits.iter_mut().zip(&wakes) {
            // Verify the Future wake contract without a timer re-polling it.
            assert!(wake.0.load(Ordering::Acquire));
            waiting.await;
        }
    }

    #[tokio::test]
    async fn cancelled_stop_wait_does_not_consume_the_request() {
        let stop = ServerStop::default();
        let mut waiting = Box::pin(stop.wait_requested());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                waiting.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        drop(waiting);
        stop.request(ShutdownReason::HostShutdown);
        stop.wait_requested().await;
        assert!(stop.is_requested());
    }
}
