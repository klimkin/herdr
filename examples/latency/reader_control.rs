//! One acknowledged outer-reader pause; private benchmark metadata only.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Condvar, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Lifecycle {
    pub client_index: usize,
    pub client_pid: u32,
    pub requested_ms: u64,
    pub installed_ns: u64,
    pub paused_ns: Option<u64>,
    pub reset_request_ns: Option<u64>,
    pub reset_complete_ns: Option<u64>,
    pub reader_resumed_ns: Option<u64>,
    pub first_read_attempt_ns: Option<u64>,
    pub first_receipt_ns: Option<u64>,
    pub stopped: bool,
}

#[derive(Default)]
struct State {
    lifecycle: Option<Lifecycle>,
    reset: bool,
    stopped: bool,
}

#[derive(Default)]
pub struct ReaderControl {
    state: Mutex<State>,
    changed: Condvar,
    waiting: AtomicBool,
    attempt_pending: AtomicBool,
    receipt_pending: AtomicBool,
}

impl ReaderControl {
    pub fn arm(
        &self,
        client_index: usize,
        client_pid: u32,
        requested_ms: u64,
    ) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        if state.lifecycle.is_some() || state.stopped {
            return Err(std::io::Error::other(
                "reader pause already armed or stopped",
            ));
        }
        state.lifecycle = Some(Lifecycle {
            client_index,
            client_pid,
            requested_ms,
            installed_ns: super::platform::monotonic_ns()?,
            paused_ns: None,
            reset_request_ns: None,
            reset_complete_ns: None,
            reader_resumed_ns: None,
            first_read_attempt_ns: None,
            first_receipt_ns: None,
            stopped: false,
        });
        self.waiting.store(true, Ordering::Release);
        Ok(())
    }

    /// Called by the reader immediately before reading, not by the controller.
    // After resume, normal reads end through owned client/server closure; stop
    // actively wakes only the gate/timer, not an already blocking system read.
    pub fn before_read(&self) -> std::io::Result<bool> {
        if !self.waiting.load(Ordering::Acquire) {
            return Ok(true);
        }
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        if state.stopped {
            return Ok(false);
        }
        if let Some(lifecycle) = &mut state.lifecycle {
            lifecycle
                .paused_ns
                .get_or_insert(super::platform::monotonic_ns()?);
        }
        self.changed.notify_all();
        while !state.reset && !state.stopped {
            state = self.changed.wait(state).map_err(|_| poisoned())?;
        }
        if state.stopped {
            return Ok(false);
        }
        if let Some(lifecycle) = &mut state.lifecycle {
            lifecycle.reader_resumed_ns = Some(super::platform::monotonic_ns()?);
        }
        self.attempt_pending.store(true, Ordering::Release);
        self.receipt_pending.store(true, Ordering::Release);
        self.waiting.store(false, Ordering::Release);
        Ok(true)
    }

    pub fn wait_paused(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        loop {
            if state.stopped {
                return Err(std::io::Error::other("reader stopped before pause"));
            }
            if state
                .lifecycle
                .as_ref()
                .is_some_and(|lifecycle| lifecycle.paused_ns.is_some())
            {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "reader pause acknowledgment timed out",
                ));
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| poisoned())?
                .0;
        }
    }

    #[cfg(test)]
    pub fn reset(&self) -> std::io::Result<()> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        self.reset_locked(state)
    }

    /// The reset timer wakes on shutdown instead of outliving the owned reader.
    pub fn reset_after(&self, duration: Duration) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        let paused = state
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.paused_ns)
            .ok_or_else(|| std::io::Error::other("reader pause not acknowledged"))?;
        let target = paused.saturating_add(duration.as_nanos().min(u128::from(u64::MAX)) as u64);
        loop {
            if state.stopped {
                return Ok(());
            }
            let remaining = target.saturating_sub(super::platform::monotonic_ns()?);
            if remaining == 0 {
                return self.reset_locked(state);
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_nanos(remaining))
                .map_err(|_| poisoned())?
                .0;
        }
    }

    fn reset_locked(&self, mut state: std::sync::MutexGuard<'_, State>) -> std::io::Result<()> {
        if state.stopped || state.reset {
            return Err(std::io::Error::other(
                "reader pause already reset or stopped",
            ));
        }
        let lifecycle = state
            .lifecycle
            .as_mut()
            .ok_or_else(|| std::io::Error::other("reader pause absent"))?;
        if lifecycle.paused_ns.is_none() {
            return Err(std::io::Error::other("reader pause not acknowledged"));
        }
        lifecycle.reset_request_ns = Some(super::platform::monotonic_ns()?);
        // Both timestamps bracket reset under this lock; reader cannot resume
        // until reset=true and the lock is released after notification.
        lifecycle.reset_complete_ns = Some(super::platform::monotonic_ns()?);
        state.reset = true;
        self.changed.notify_all();
        Ok(())
    }

    pub fn read_attempt(&self) -> std::io::Result<()> {
        if self.attempt_pending.swap(false, Ordering::AcqRel) {
            let ns = super::platform::monotonic_ns()?;
            let mut state = self.state.lock().map_err(|_| poisoned())?;
            if let Some(lifecycle) = &mut state.lifecycle {
                lifecycle.first_read_attempt_ns = Some(ns);
            }
        }
        Ok(())
    }

    pub fn received(&self, ns: u64) {
        if self.receipt_pending.swap(false, Ordering::AcqRel) {
            if let Ok(mut state) = self.state.lock() {
                if let Some(lifecycle) = &mut state.lifecycle {
                    lifecycle.first_receipt_ns = Some(ns);
                }
            }
        }
    }

    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            if let Some(lifecycle) = &mut state.lifecycle {
                lifecycle.stopped = true;
            }
            self.changed.notify_all();
        }
    }

    pub fn snapshot(&self) -> std::io::Result<Option<Lifecycle>> {
        Ok(self.state.lock().map_err(|_| poisoned())?.lifecycle.clone())
    }
}

fn poisoned() -> std::io::Error {
    std::io::Error::other("reader pause lock poisoned")
}
