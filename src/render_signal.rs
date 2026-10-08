#[cfg(any(test, feature = "latency-experiments"))]
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::layout::PaneId;

#[derive(Debug, Default)]
pub(crate) struct RenderRequest {
    pub(crate) generic: bool,
    pub(crate) pty_sources: HashSet<PaneId>,
    pub(crate) terminal_title_sources: HashSet<PaneId>,
}

/// Owns one detached request. Unhandled work returns to the signal without a
/// notification; the server retains its ordinary render scheduling obligation.
pub(crate) struct PendingRenderRequest {
    pub(crate) request: RenderRequest,
    signal: Arc<RenderSignal>,
}

impl PendingRenderRequest {
    /// Completes ordinary classification or transfers unsent work to the
    /// existing per-client full-render recovery path.
    pub(crate) fn complete(mut self) {
        self.request = RenderRequest::default();
    }
}

impl Drop for PendingRenderRequest {
    fn drop(&mut self) {
        if !self.request.generic
            && self.request.pty_sources.is_empty()
            && self.request.terminal_title_sources.is_empty()
        {
            return;
        }
        let mut state = self
            .signal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.request.generic |= self.request.generic;
        state
            .request
            .pty_sources
            .extend(self.request.pty_sources.drain());
        state
            .request
            .terminal_title_sources
            .extend(self.request.terminal_title_sources.drain());
        self.signal.pending.store(true, Ordering::Release);
    }
}

/// Coalesces render requests while retaining enough origin information for the
/// headless server to discard PTY-only updates hidden from every client.
#[derive(Debug, Default)]
pub(crate) struct RenderSignal {
    pending: AtomicBool,
    state: Mutex<RenderSignalState>,
}

#[derive(Debug, Default)]
struct RenderSignalState {
    request: RenderRequest,
    immediate_pty_sources: HashSet<PaneId>,
    #[cfg(any(test, feature = "latency-experiments"))]
    target_ready: HashMap<PaneId, TargetReady>,
}

#[derive(Debug)]
#[cfg(any(test, feature = "latency-experiments"))]
struct TargetReady {
    instance: u64,
    baseline_revision: u64,
    expires_at: std::time::Instant,
    opportunity: u64,
    armed: bool,
    ready_revision: u64,
}

impl RenderSignal {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn request_generic(&self) {
        crate::latency_prof::record("render.state_ready", 0, 0);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.request.generic = true;
        self.pending.store(true, Ordering::Release);
    }

    /// Returns true when the signal becomes pending or visible PTY work joins it.
    pub(crate) fn request_pty(&self, pane_id: PaneId) -> bool {
        self.publish_pty(pane_id, None)
    }

    /// Publishes only renderable state through the existing PTY signal. An
    /// accepted input may wake once even if bulk output already owns the source.
    pub(crate) fn request_pty_ready(&self, pane_id: PaneId, instance: u64, revision: u64) -> bool {
        self.publish_pty(pane_id, Some((instance, revision)))
    }

    fn publish_pty(&self, pane_id: PaneId, ready: Option<(u64, u64)>) -> bool {
        crate::latency_prof::record("render.state_ready", pane_id.raw() as u64, 0);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source_added = state.request.pty_sources.insert(pane_id);
        let wake_for_source = source_added && state.immediate_pty_sources.contains(&pane_id);
        // No registered target means no clock read, terminal access or allocation.
        #[cfg(any(test, feature = "latency-experiments"))]
        let mut wake_for_target = false;
        #[cfg(not(any(test, feature = "latency-experiments")))]
        let wake_for_target = false;
        #[cfg(any(test, feature = "latency-experiments"))]
        if let (Some((instance, revision)), Some(target)) =
            (ready, state.target_ready.get_mut(&pane_id))
        {
            if target.instance == instance && revision.is_multiple_of(2) {
                target.ready_revision = target.ready_revision.max(revision);
            }
            if target.armed && target.instance == instance {
                if std::time::Instant::now() >= target.expires_at {
                    target.armed = false;
                } else if revision.is_multiple_of(2) && revision > target.baseline_revision {
                    target.armed = false;
                    wake_for_target = true;
                    crate::latency_prof::record_runtime_at(
                        "render.target_ready_wake",
                        pane_id.raw() as u64,
                        revision,
                        target.opportunity,
                        crate::latency_prof::now(),
                        instance,
                    );
                }
            }
        }
        #[cfg(not(any(test, feature = "latency-experiments")))]
        let _ = ready;
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || wake_for_source || wake_for_target
    }

    /// Repeated input shares the first expiry and cannot rearm consumed output.
    pub(crate) fn arm_target_ready(
        &self,
        pane_id: PaneId,
        instance: u64,
        baseline_revision: u64,
        expires_at: std::time::Instant,
        opportunity: u64,
    ) {
        #[cfg(any(test, feature = "latency-experiments"))]
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .target_ready
                .get(&pane_id)
                .is_some_and(|target| target.opportunity == opportunity)
            {
                return;
            }
            state.target_ready.insert(
                pane_id,
                TargetReady {
                    instance,
                    baseline_revision,
                    expires_at,
                    opportunity,
                    armed: true,
                    ready_revision: baseline_revision,
                },
            );
        }
        #[cfg(not(any(test, feature = "latency-experiments")))]
        let _ = (
            pane_id,
            instance,
            baseline_revision,
            expires_at,
            opportunity,
        );
    }

    /// Ordinary delivery keeps unused feedback eligible for newer output.
    /// Ready output can race before rearming, even while its source is detached.
    pub(crate) fn advance_target_ready(
        &self,
        advance: &crate::app::early_presentation::TerminalFeedbackAdvance,
    ) -> bool {
        #[cfg(any(test, feature = "latency-experiments"))]
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(target) = state.target_ready.get_mut(&advance.pane_id) else {
                return false;
            };
            if target.opportunity != advance.opportunity
                || target.instance != advance.runtime_instance
                || advance.revision <= target.baseline_revision
                || !advance.revision.is_multiple_of(2)
            {
                return false;
            }
            if std::time::Instant::now() >= target.expires_at {
                target.armed = false;
                return false;
            }
            target.baseline_revision = advance.revision;
            let ready = target.ready_revision;
            let wake = ready > advance.revision;
            target.armed = !wake;
            if wake {
                state.request.pty_sources.insert(advance.pane_id);
                self.pending.store(true, Ordering::Release);
                crate::latency_prof::record_runtime_at(
                    "render.target_ready_wake",
                    advance.pane_id.raw() as u64,
                    ready,
                    advance.opportunity,
                    crate::latency_prof::now(),
                    advance.runtime_instance,
                );
            }
            wake
        }
        #[cfg(not(any(test, feature = "latency-experiments")))]
        {
            let _ = (
                advance.pane_id,
                advance.runtime_instance,
                advance.revision,
                advance.opportunity,
            );
            false
        }
    }

    /// Policy retirement owns latch lifetime, including sources that stop output.
    #[cfg(test)]
    pub(crate) fn cancel_target_ready(&self, pane_id: PaneId, opportunity: u64) {
        #[cfg(any(test, feature = "latency-experiments"))]
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .target_ready
                .get(&pane_id)
                .is_some_and(|target| target.opportunity == opportunity)
            {
                state.target_ready.remove(&pane_id);
            }
        }
        #[cfg(not(any(test, feature = "latency-experiments")))]
        let _ = (pane_id, opportunity);
    }

    pub(crate) fn prune_target_ready(&self, mut keep: impl FnMut(u64) -> bool) {
        #[cfg(any(test, feature = "latency-experiments"))]
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target_ready
            .retain(|_, target| keep(target.opportunity));
        #[cfg(not(any(test, feature = "latency-experiments")))]
        let _ = &mut keep;
    }

    pub(crate) fn has_pending_source(&self, pane_id: PaneId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request
            .pty_sources
            .contains(&pane_id)
    }

    pub(crate) fn set_immediate_pty_sources(&self, sources: HashSet<PaneId>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .immediate_pty_sources = sources;
    }

    pub(crate) fn has_immediate_work(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.request.generic
            || !state.request.terminal_title_sources.is_empty()
            || state
                .request
                .pty_sources
                .iter()
                .any(|pane_id| state.immediate_pty_sources.contains(pane_id))
    }

    /// Coalesces terminal-title changes separately from ordinary PTY damage so
    /// consumers can update metadata without inspecting every pane.
    pub(crate) fn request_terminal_title(&self, pane_id: PaneId) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source_added = state.request.terminal_title_sources.insert(pane_id);
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || source_added
    }

    pub(crate) fn pending_terminal_title_sources(&self) -> HashSet<PaneId> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request
            .terminal_title_sources
            .clone()
    }

    pub(crate) fn take(&self) -> RenderRequest {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.pending.store(false, Ordering::Release);
        std::mem::take(&mut state.request)
    }

    pub(crate) fn take_pending(self: &Arc<Self>) -> PendingRenderRequest {
        PendingRenderRequest {
            request: self.take(),
            signal: Arc::clone(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_pty_sources_until_taken() {
        let signal = RenderSignal::new();
        let first = PaneId::from_raw(10);
        let second = PaneId::from_raw(20);

        assert!(signal.request_pty(first));
        assert!(!signal.request_pty(first));
        assert!(!signal.request_pty(second));

        let request = signal.take();
        assert!(!request.generic);
        assert_eq!(request.pty_sources, HashSet::from([first, second]));
        assert!(request.terminal_title_sources.is_empty());
        assert!(!signal.is_pending());
    }

    #[test]
    fn hidden_pty_sources_coalesce_to_one_wake() {
        let signal = RenderSignal::new();
        signal.set_immediate_pty_sources(HashSet::from([PaneId::from_raw(100)]));

        let wakes = (1..=50)
            .filter(|pane_id| signal.request_pty(PaneId::from_raw(*pane_id)))
            .count();

        assert_eq!(wakes, 1);
    }

    #[test]
    fn immediate_pty_source_wakes_pending_hidden_work() {
        let signal = RenderSignal::new();
        let hidden = PaneId::from_raw(10);
        let visible = PaneId::from_raw(20);
        signal.set_immediate_pty_sources(HashSet::from([visible]));

        assert!(signal.request_pty(hidden));
        assert!(!signal.request_pty(PaneId::from_raw(30)));
        assert!(signal.request_pty(visible));
        assert!(!signal.request_pty(visible));
    }

    #[test]
    fn terminal_title_source_wakes_pending_pty_work() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        assert!(signal.request_pty(pane_id));
        assert!(signal.request_terminal_title(pane_id));
        assert!(!signal.request_terminal_title(pane_id));
    }

    #[test]
    fn coalesces_terminal_title_sources_without_making_them_pty_damage() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        assert!(signal.request_terminal_title(pane_id));
        assert!(!signal.request_terminal_title(pane_id));
        assert_eq!(
            signal.pending_terminal_title_sources(),
            HashSet::from([pane_id])
        );

        let request = signal.take();
        assert!(request.pty_sources.is_empty());
        assert_eq!(request.terminal_title_sources, HashSet::from([pane_id]));
    }

    #[test]
    fn keeps_generic_and_pty_requests_distinct() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        signal.request_generic();
        assert!(!signal.request_pty(pane_id));

        let request = signal.take();
        assert!(request.generic);
        assert_eq!(request.pty_sources, HashSet::from([pane_id]));
    }

    #[test]
    fn completing_one_terminal_keeps_other_sources_titles_and_generic_work_pending() {
        let signal = Arc::new(RenderSignal::new());
        let selected = PaneId::from_raw(10);
        let unrelated = PaneId::from_raw(20);
        signal.request_generic();
        signal.request_pty(selected);
        signal.request_pty(unrelated);
        signal.request_terminal_title(selected);

        let mut pending = signal.take_pending();
        pending.request.pty_sources.remove(&selected);
        drop(pending);

        let remaining = signal.take_pending();
        assert!(remaining.request.generic);
        assert_eq!(remaining.request.pty_sources, HashSet::from([unrelated]));
        assert_eq!(
            remaining.request.terminal_title_sources,
            HashSet::from([selected])
        );
        remaining.complete();
        assert!(!signal.is_pending());
    }

    #[test]
    fn restoring_detached_work_preserves_later_producer_arrivals() {
        let signal = Arc::new(RenderSignal::new());
        let selected = PaneId::from_raw(10);
        let residual = PaneId::from_raw(20);
        let later = PaneId::from_raw(30);
        signal.set_immediate_pty_sources(HashSet::from([selected]));
        signal.request_pty(selected);
        signal.request_pty(residual);
        let mut pending = signal.take_pending();

        let (published, received) = std::sync::mpsc::channel();
        let producer_signal = Arc::clone(&signal);
        let producer = std::thread::spawn(move || {
            producer_signal.request_pty(selected);
            producer_signal.request_pty(later);
            producer_signal.request_generic();
            producer_signal.request_terminal_title(later);
            published.send(()).expect("publish arrivals");
        });
        received.recv().expect("later arrivals published");
        pending.request.pty_sources.remove(&selected);
        drop(pending);
        producer.join().expect("producer completed");

        let remaining = signal.take_pending();
        assert!(remaining.request.generic);
        assert_eq!(
            remaining.request.pty_sources,
            HashSet::from([selected, residual, later])
        );
        assert_eq!(
            remaining.request.terminal_title_sources,
            HashSet::from([later])
        );
        signal.request_pty(selected);
        remaining.complete();
        let newest = signal.take_pending();
        assert!(!newest.request.generic);
        assert_eq!(newest.request.pty_sources, HashSet::from([selected]));
        assert!(newest.request.terminal_title_sources.is_empty());
        newest.complete();
        assert!(!signal.is_pending());
    }

    #[test]
    fn accepted_input_wakes_already_pending_target_once_after_new_ready_state() {
        let signal = RenderSignal::new();
        let pane = PaneId::from_raw(10);
        assert!(signal.request_pty(pane));
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(10);
        signal.arm_target_ready(pane, 7, 20, expiry, 1);
        assert!(!signal.request_pty_ready(pane, 7, 20));
        assert!(!signal.request_pty_ready(pane, 8, 22));
        assert!(signal.request_pty_ready(pane, 7, 22));
        assert!(!signal.request_pty_ready(pane, 7, 24));
        // Repeated accepted input shares the same opportunity, never rearming
        // the one ready wake already consumed by output.
        signal.arm_target_ready(pane, 7, 24, expiry, 1);
        assert!(!signal.request_pty_ready(pane, 7, 26));
        assert_eq!(signal.take().pty_sources, HashSet::from([pane]));
    }

    #[test]
    fn ordinary_delivery_rearms_feedback_without_losing_concurrent_ready_output() {
        use crate::app::early_presentation::TerminalFeedbackAdvance;

        let signal = Arc::new(RenderSignal::new());
        let pane = PaneId::from_raw(10);
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(10);
        signal.arm_target_ready(pane, 7, 20, expiry, 1);
        assert!(signal.request_pty_ready(pane, 7, 22));
        let ordinary = signal.take_pending();
        assert!(signal.request_pty_ready(pane, 7, 24));
        ordinary.complete();
        let advance = TerminalFeedbackAdvance {
            pane_id: pane,
            runtime_instance: 7,
            revision: 22,
            opportunity: 1,
        };
        assert!(signal.advance_target_ready(&advance));
        assert!(signal.is_pending());
        assert!(signal.has_pending_source(pane));
        assert!(
            !signal.advance_target_ready(&advance),
            "duplicate peer receipt cannot rearm"
        );
        assert!(
            !signal.request_pty_ready(pane, 7, 26),
            "ready wake remains one-shot"
        );

        let delivered = TerminalFeedbackAdvance {
            revision: 26,
            ..advance
        };
        assert!(!signal.advance_target_ready(&delivered));
        assert!(
            !signal.request_pty_ready(pane, 7, 26),
            "unchanged state cannot wake"
        );
        assert!(
            signal.request_pty_ready(pane, 7, 28),
            "later output wakes after floor advance"
        );
    }

    #[test]
    fn ordinary_delivery_restores_ready_output_even_after_detached_work_completes() {
        use crate::app::early_presentation::TerminalFeedbackAdvance;

        let signal = Arc::new(RenderSignal::new());
        let pane = PaneId::from_raw(10);
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(10);
        signal.arm_target_ready(pane, 7, 20, expiry, 1);
        assert!(signal.request_pty_ready(pane, 7, 22));
        assert!(!signal.request_pty_ready(pane, 7, 24));
        signal.take_pending().complete();
        assert!(!signal.is_pending());
        let advance = TerminalFeedbackAdvance {
            pane_id: pane,
            runtime_instance: 7,
            revision: 22,
            opportunity: 1,
        };
        assert!(signal.advance_target_ready(&advance));
        assert!(signal.has_pending_source(pane));
        assert_eq!(signal.take().pty_sources, HashSet::from([pane]));
        assert!(!signal.advance_target_ready(&advance));
        // Coalesced input may still carry an earlier captured baseline.
        signal.request_pty(pane);
        signal.arm_target_ready(pane, 7, 20, expiry, 1);
        assert!(!signal.request_pty_ready(pane, 7, 26));
        assert!(!signal.request_pty_ready(pane, 7, 28));
    }

    #[test]
    fn stale_or_invalid_delivery_cannot_rearm_feedback() {
        use crate::app::early_presentation::TerminalFeedbackAdvance;

        let signal = RenderSignal::new();
        let pane = PaneId::from_raw(10);
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(10);
        signal.arm_target_ready(pane, 7, 20, expiry, 2);
        signal.request_pty_ready(pane, 7, 22);
        let valid = TerminalFeedbackAdvance {
            pane_id: pane,
            runtime_instance: 7,
            revision: 22,
            opportunity: 2,
        };
        for advance in [
            TerminalFeedbackAdvance {
                opportunity: 1,
                ..valid
            },
            TerminalFeedbackAdvance {
                runtime_instance: 8,
                ..valid
            },
            TerminalFeedbackAdvance {
                revision: 23,
                ..valid
            },
            TerminalFeedbackAdvance {
                revision: 20,
                ..valid
            },
        ] {
            assert!(!signal.advance_target_ready(&advance));
        }
        assert!(!signal.request_pty_ready(pane, 7, 24));
        assert!(signal.advance_target_ready(&valid));
        assert!(!signal.advance_target_ready(&valid));
        signal.cancel_target_ready(pane, 2);
        assert!(!signal.advance_target_ready(&TerminalFeedbackAdvance {
            revision: 26,
            ..valid
        }));
        signal.arm_target_ready(pane, 7, 26, std::time::Instant::now(), 3);
        assert!(!signal.advance_target_ready(&TerminalFeedbackAdvance {
            revision: 28,
            opportunity: 3,
            ..valid
        }));
    }

    #[test]
    fn expired_or_canceled_input_does_not_wake_pending_output() {
        let signal = RenderSignal::new();
        let pane = PaneId::from_raw(10);
        signal.request_pty(pane);
        signal.arm_target_ready(pane, 7, 20, std::time::Instant::now(), 1);
        assert!(!signal.request_pty_ready(pane, 7, 22));
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(10);
        signal.arm_target_ready(pane, 7, 22, expiry, 2);
        signal.cancel_target_ready(pane, 1);
        assert!(signal.request_pty_ready(pane, 7, 24));
        signal.arm_target_ready(pane, 7, 24, expiry, 3);
        signal.cancel_target_ready(pane, 3);
        assert!(!signal.request_pty_ready(pane, 7, 26));
    }
}
