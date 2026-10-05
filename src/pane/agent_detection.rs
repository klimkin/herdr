use std::sync::atomic::{AtomicU64, Ordering};

use crate::detect::{Agent, AgentDetection, AgentState};

pub(super) const TICK_UNIDENTIFIED: std::time::Duration = std::time::Duration::from_millis(500);
pub(super) const TICK_IDENTIFIED: std::time::Duration = std::time::Duration::from_millis(300);
const TICK_PENDING_RELEASE: std::time::Duration = std::time::Duration::from_millis(50);

pub(super) fn detection_poll_interval(
    agent: Option<Agent>,
    pending_idle: bool,
    pending_release: bool,
) -> std::time::Duration {
    if pending_release {
        TICK_PENDING_RELEASE
    } else if pending_idle {
        AGENT_PENDING_IDLE_RECHECK
    } else if agent.is_none() {
        TICK_UNIDENTIFIED
    } else {
        TICK_IDENTIFIED
    }
}

pub(super) const AGENT_PENDING_IDLE_RECHECK: std::time::Duration =
    std::time::Duration::from_millis(100);
const AGENT_PENDING_IDLE_CONFIRMATIONS: u8 = 3;
pub(super) const AGENT_PENDING_IDLE_CAP: std::time::Duration =
    std::time::Duration::from_millis(700);
pub(super) const STABLE_VISIBLE_SIGNAL_REFRESH: std::time::Duration =
    std::time::Duration::from_millis(800);
pub(super) const AGENT_STARTUP_GRACE_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetectionPublishState {
    pub(super) state: AgentState,
    pub(super) visible_idle: bool,
    pub(super) visible_blocker: bool,
    pub(super) visible_working: bool,
}

#[derive(Debug, Default)]
pub(super) struct PendingIdleConfirmation {
    started_at: Option<std::time::Instant>,
    confirmations: u8,
}

impl PendingIdleConfirmation {
    pub(super) fn active(&self) -> bool {
        self.started_at.is_some()
    }

    pub(super) fn clear(&mut self) {
        self.started_at = None;
        self.confirmations = 0;
    }

    pub(super) fn should_hold_working_to_idle(
        &mut self,
        previous: DetectionPublishState,
        next: DetectionPublishState,
        agent_changed: bool,
        process_exited: bool,
        now: std::time::Instant,
    ) -> bool {
        let is_working_to_plain_idle = previous.state == AgentState::Working
            && next.state == AgentState::Idle
            && !next.visible_idle
            && !next.visible_blocker
            && !agent_changed
            && !process_exited;

        if !is_working_to_plain_idle {
            self.clear();
            return false;
        }

        let Some(started_at) = self.started_at else {
            self.started_at = Some(now);
            self.confirmations = 0;
            return true;
        };

        if now.duration_since(started_at) >= AGENT_PENDING_IDLE_CAP {
            self.clear();
            return false;
        }

        self.confirmations = self.confirmations.saturating_add(1);
        if self.confirmations >= AGENT_PENDING_IDLE_CONFIRMATIONS {
            self.clear();
            return false;
        }

        true
    }
}

/// Retains bottom-buffer evidence between ticks; deadlines still run every tick.
#[derive(Debug, Default)]
pub(super) struct DetectionScreenCache {
    key: Option<(Option<Agent>, u64)>,
    content: String,
    detection: Option<AgentDetection>,
}

pub(super) struct DetectionScreenSnapshot {
    pub(super) content: String,
    pub(super) detection: Option<AgentDetection>,
}

pub(super) struct DetectionScreenObservation {
    pub(super) detection: Option<AgentDetection>,
    pub(super) content_changed: bool,
    pub(super) refreshed: bool,
}

impl DetectionScreenCache {
    pub(super) fn invalidate(&mut self) {
        self.key = None;
    }

    pub(super) fn content(&self) -> &str {
        &self.content
    }

    pub(super) fn observe(
        &mut self,
        agent: Option<Agent>,
        content_seq: u64,
        process_exited: bool,
        read_snapshot: impl FnOnce() -> DetectionScreenSnapshot,
    ) -> DetectionScreenObservation {
        if process_exited {
            return DetectionScreenObservation {
                detection: detection_update_for_publish_with_osc(agent, "", "", "", true),
                content_changed: false,
                refreshed: false,
            };
        }

        let refreshed = self.key != Some((agent, content_seq));
        let mut content_changed = false;
        if refreshed {
            let snapshot = read_snapshot();
            // Raw PTY generations include invisible controls. Only rendered-text
            // changes count as evidence for same-process-group acquisition.
            content_changed = snapshot.content != self.content;
            self.content = snapshot.content;
            self.detection = snapshot.detection;
            self.key = Some((agent, content_seq));
        }
        DetectionScreenObservation {
            detection: self.detection,
            content_changed,
            refreshed,
        }
    }
}

pub(super) fn should_publish_detection_update(
    previous: DetectionPublishState,
    next: DetectionPublishState,
    agent_changed: bool,
    process_exited: bool,
    stable_visible_signal_refresh_due: bool,
) -> bool {
    next.state != previous.state
        || next.visible_idle != previous.visible_idle
        || next.visible_blocker != previous.visible_blocker
        || next.visible_working != previous.visible_working
        || agent_changed
        || process_exited
        || (stable_visible_signal_refresh_due && next.visible_blocker && previous.visible_blocker)
}

pub(super) fn stable_visible_signal_refresh_due(
    previous: DetectionPublishState,
    next: DetectionPublishState,
    last_refresh: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let stable_visible_signal = next.visible_blocker && previous.visible_blocker;

    stable_visible_signal
        && last_refresh.is_none_or(|last_refresh| {
            now.duration_since(last_refresh) >= STABLE_VISIBLE_SIGNAL_REFRESH
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionTransitionDecision {
    NoPublish,
    PublishNext,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DetectionTransitionInput {
    pub(super) previous_publish: DetectionPublishState,
    pub(super) next_publish: DetectionPublishState,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) stable_refresh_due: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_detection_transition(
    input: DetectionTransitionInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionTransitionDecision {
    if pending_idle.should_hold_working_to_idle(
        input.previous_publish,
        input.next_publish,
        input.agent_changed,
        input.process_exited,
        input.now,
    ) {
        return DetectionTransitionDecision::NoPublish;
    }

    if should_publish_detection_update(
        input.previous_publish,
        input.next_publish,
        input.agent_changed,
        input.process_exited,
        input.stable_refresh_due,
    ) {
        return DetectionTransitionDecision::PublishNext;
    }

    DetectionTransitionDecision::NoPublish
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionPublishDecision {
    NoPublish,
    Publish {
        state: AgentState,
        visible_idle: bool,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenDetectionPublishInput {
    pub(super) current_state: AgentState,
    pub(super) last_visible_idle: bool,
    pub(super) last_visible_blocker: bool,
    pub(super) last_visible_working: bool,
    pub(super) last_visible_signal_refresh: Option<std::time::Instant>,
    pub(super) screen_detection: AgentDetection,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_screen_detection_publish(
    input: ScreenDetectionPublishInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionPublishDecision {
    let detection = input.screen_detection;
    let new_state = crate::terminal::state::stabilize_agent_detection(detection);
    let visible_idle = detection.visible_idle && new_state == AgentState::Idle;
    let visible_blocker = detection.visible_blocker && new_state == AgentState::Blocked;
    let visible_working = detection.visible_working && new_state == AgentState::Working;

    let previous_publish = DetectionPublishState {
        state: input.current_state,
        visible_idle: input.last_visible_idle,
        visible_blocker: input.last_visible_blocker,
        visible_working: input.last_visible_working,
    };
    let next_publish = DetectionPublishState {
        state: new_state,
        visible_idle,
        visible_blocker,
        visible_working,
    };
    let stable_refresh_due = stable_visible_signal_refresh_due(
        previous_publish,
        next_publish,
        input.last_visible_signal_refresh,
        input.now,
    );

    match decide_detection_transition(
        DetectionTransitionInput {
            previous_publish,
            next_publish,
            agent_changed: input.agent_changed,
            process_exited: input.process_exited,
            stable_refresh_due,
            now: input.now,
        },
        pending_idle,
    ) {
        DetectionTransitionDecision::NoPublish => DetectionPublishDecision::NoPublish,
        DetectionTransitionDecision::PublishNext => DetectionPublishDecision::Publish {
            state: new_state,
            visible_idle,
            visible_blocker,
            visible_working,
            process_exited: input.process_exited,
        },
    }
}

#[allow(dead_code)] // shim for tests; detection_update_for_publish_with_osc is the real path
pub(super) fn detection_update_for_publish(
    agent: Option<Agent>,
    content: &str,
    process_exited: bool,
) -> Option<crate::detect::AgentDetection> {
    detection_update_for_publish_with_osc(agent, content, "", "", process_exited)
}

pub(super) fn detection_update_for_publish_with_osc(
    agent: Option<Agent>,
    content: &str,
    osc_title: &str,
    osc_progress: &str,
    process_exited: bool,
) -> Option<crate::detect::AgentDetection> {
    if process_exited {
        return Some(crate::detect::AgentDetection {
            state: AgentState::Idle,
            skip_state_update: false,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
        });
    }

    let detection = crate::detect::detect_agent_with_osc(agent, content, osc_title, osc_progress);
    (!detection.skip_state_update).then_some(detection)
}

pub(super) fn codex_prompt_ready(content: &str) -> bool {
    // The composer can remain visible during a turn; this is startup evidence only.
    let recent: String = content
        .lines()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(str::chars)
        .filter(|ch| !ch.is_whitespace())
        .collect();
    recent.contains("›AskCodextodoanything")
        && !recent.contains("model:loading")
        && !recent.contains("Resumingsession")
}

pub(super) fn observe_detection_content_change(bytes: &[u8], detection_content_seq: &AtomicU64) {
    if !bytes.is_empty() {
        detection_content_seq.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn mark_detection_content_changed(detection_content_seq: &AtomicU64) {
    detection_content_seq.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_state(state: AgentState) -> DetectionPublishState {
        DetectionPublishState {
            state,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        }
    }

    fn screen_detection(state: AgentState) -> AgentDetection {
        AgentDetection {
            state,
            skip_state_update: false,
            visible_idle: state == AgentState::Idle,
            visible_blocker: false,
            visible_working: state == AgentState::Working,
        }
    }

    fn screen_publish_input(
        current_state: AgentState,
        screen_detection: AgentDetection,
        now: std::time::Instant,
    ) -> ScreenDetectionPublishInput {
        ScreenDetectionPublishInput {
            current_state,
            last_visible_idle: false,
            last_visible_blocker: false,
            last_visible_working: false,
            last_visible_signal_refresh: None,
            screen_detection,
            process_exited: false,
            agent_changed: false,
            now,
        }
    }

    #[test]
    fn codex_startup_prompt_survives_terminal_wraps() {
        let wrapped = "header\n› Ask Codex to do\nanything\nfooter";
        assert!(codex_prompt_ready(wrapped));
        assert!(!codex_prompt_ready(&format!("model: load\ning\n{wrapped}")));
    }

    #[test]
    fn screen_cache_formats_quiet_panes_once_for_every_state() {
        for agent in [None, Some(Agent::Codex), Some(Agent::Pi)] {
            for state in [
                AgentState::Idle,
                AgentState::Working,
                AgentState::Blocked,
                AgentState::Unknown,
            ] {
                let mut cache = DetectionScreenCache::default();
                let mut reads = 0;
                for _ in 0..20 {
                    let observation = cache.observe(agent, 10, false, || {
                        reads += 1;
                        DetectionScreenSnapshot {
                            content: "unchanged".into(),
                            detection: Some(screen_detection(state)),
                        }
                    });
                    assert_eq!(observation.detection.unwrap().state, state);
                }
                assert_eq!(reads, 1, "agent={agent:?}, state={state:?}");
            }
        }
    }

    #[test]
    fn screen_cache_invisible_output_preserves_semantic_content_change() {
        let mut cache = DetectionScreenCache::default();
        for (seq, expected_change) in [(1, true), (2, false), (2, false)] {
            let observation = cache.observe(None, seq, false, || DetectionScreenSnapshot {
                content: "same bottom buffer".into(),
                detection: Some(screen_detection(AgentState::Unknown)),
            });
            assert_eq!(observation.content_changed, expected_change);
        }
    }

    #[test]
    fn screen_cache_rechecks_new_generation_agent_and_reset() {
        let mut cache = DetectionScreenCache::default();
        let mut reads = 0;
        for (agent, seq, reset) in [
            (Some(Agent::Codex), 1, false),
            (Some(Agent::Codex), 2, false),
            (Some(Agent::Pi), 2, false),
            (Some(Agent::Pi), 2, true),
        ] {
            if reset {
                cache.invalidate();
            }
            let observation = cache.observe(agent, seq, false, || {
                reads += 1;
                DetectionScreenSnapshot {
                    content: "retained screen".into(),
                    detection: Some(screen_detection(AgentState::Working)),
                }
            });
            assert!(observation.refreshed);
        }
        assert_eq!(reads, 4);
    }

    #[test]
    fn screen_cache_retains_skip_state_until_content_or_reset_changes() {
        let mut cache = DetectionScreenCache::default();
        let mut reads = 0;
        for _ in 0..4 {
            let observation = cache.observe(Some(Agent::Pi), 1, false, || {
                reads += 1;
                DetectionScreenSnapshot {
                    content: "historical view".into(),
                    detection: None,
                }
            });
            assert!(observation.detection.is_none());
        }
        assert_eq!(reads, 1);
        cache.invalidate();
        let observation = cache.observe(Some(Agent::Pi), 1, false, || DetectionScreenSnapshot {
            content: "historical view".into(),
            detection: Some(screen_detection(AgentState::Working)),
        });
        assert!(observation.refreshed);
        assert!(!observation.content_changed);
        assert_eq!(observation.detection.unwrap().state, AgentState::Working);
    }

    #[test]
    fn screen_cache_process_exit_publishes_without_formatting() {
        let mut cache = DetectionScreenCache::default();
        let observation = cache.observe(Some(Agent::Codex), 0, true, || {
            panic!("process exit must not format a screen")
        });
        assert_eq!(
            observation.detection.unwrap(),
            screen_detection(AgentState::Idle)
        );
        assert!(!observation.content_changed);
        assert!(!observation.refreshed);
    }

    #[test]
    fn screen_cache_rechecks_osc_only_evidence_on_new_generation() {
        let mut cache = DetectionScreenCache::default();
        for (seq, state) in [(1, AgentState::Working), (2, AgentState::Blocked)] {
            let observation = cache.observe(Some(Agent::Codex), seq, false, || {
                // Synthetic classifier changes only OSC evidence; screen stays fixed.
                DetectionScreenSnapshot {
                    content: "retained screen".into(),
                    detection: Some(screen_detection(state)),
                }
            });
            assert_eq!(observation.detection.unwrap().state, state);
            assert_eq!(observation.content_changed, seq == 1);
        }
    }

    #[test]
    fn screen_cache_keeps_plain_idle_confirmations_without_new_snapshots() {
        let mut cache = DetectionScreenCache::default();
        let mut pending = PendingIdleConfirmation::default();
        let now = std::time::Instant::now();
        let mut reads = 0;
        for tick in 0..=3 {
            let observation = cache.observe(Some(Agent::Pi), 1, false, || {
                reads += 1;
                DetectionScreenSnapshot {
                    content: "ready".into(),
                    detection: Some(AgentDetection {
                        visible_idle: false,
                        ..screen_detection(AgentState::Idle)
                    }),
                }
            });
            let decision = decide_screen_detection_publish(
                screen_publish_input(
                    AgentState::Working,
                    observation.detection.unwrap(),
                    now + AGENT_PENDING_IDLE_RECHECK * tick,
                ),
                &mut pending,
            );
            if tick < 3 {
                assert_eq!(decision, DetectionPublishDecision::NoPublish);
            } else {
                assert!(matches!(
                    decision,
                    DetectionPublishDecision::Publish {
                        state: AgentState::Idle,
                        ..
                    }
                ));
            }
        }
        assert_eq!(reads, 1);
    }

    #[test]
    fn screen_cache_keeps_blocker_refresh_deadline_without_new_snapshots() {
        let mut cache = DetectionScreenCache::default();
        let mut pending = PendingIdleConfirmation::default();
        let now = std::time::Instant::now();
        let mut reads = 0;
        for elapsed in [std::time::Duration::ZERO, STABLE_VISIBLE_SIGNAL_REFRESH] {
            let observation = cache.observe(Some(Agent::Pi), 1, false, || {
                reads += 1;
                DetectionScreenSnapshot {
                    content: "permission".into(),
                    detection: Some(AgentDetection {
                        visible_blocker: true,
                        ..screen_detection(AgentState::Blocked)
                    }),
                }
            });
            let decision = decide_screen_detection_publish(
                ScreenDetectionPublishInput {
                    last_visible_blocker: true,
                    last_visible_signal_refresh: Some(now),
                    ..screen_publish_input(
                        AgentState::Blocked,
                        observation.detection.unwrap(),
                        now + elapsed,
                    )
                },
                &mut pending,
            );
            assert_eq!(
                matches!(decision, DetectionPublishDecision::Publish { .. }),
                !elapsed.is_zero()
            );
        }
        assert_eq!(reads, 1);
    }

    #[test]
    fn detection_poll_interval_prioritizes_release_and_idle_confirmation() {
        assert_eq!(
            detection_poll_interval(None, false, false),
            TICK_UNIDENTIFIED
        );
        assert_eq!(
            detection_poll_interval(Some(Agent::Pi), false, false),
            TICK_IDENTIFIED
        );
        assert_eq!(
            detection_poll_interval(None, true, false),
            AGENT_PENDING_IDLE_RECHECK
        );
        assert_eq!(
            detection_poll_interval(None, true, true),
            TICK_PENDING_RELEASE
        );
    }

    #[test]
    fn pending_idle_holds_working_to_plain_idle_until_confirmed() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let next = publish_state(AgentState::Idle);
        let mut pending = PendingIdleConfirmation::default();

        assert!(pending.should_hold_working_to_idle(previous, next, false, false, now));
        assert!(pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK
        ));
        assert!(pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 2
        ));
        assert!(!pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 3
        ));
    }

    #[test]
    fn visible_idle_bypasses_plain_idle_hold() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let mut next = publish_state(AgentState::Idle);
        next.visible_idle = true;
        let mut pending = PendingIdleConfirmation::default();

        assert!(!pending.should_hold_working_to_idle(previous, next, false, false, now));
    }

    #[test]
    fn transition_decision_publishes_next_for_visible_blocker() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();
        let mut blocked = publish_state(AgentState::Blocked);
        blocked.visible_blocker = true;

        assert_eq!(
            decide_detection_transition(
                DetectionTransitionInput {
                    previous_publish: publish_state(AgentState::Idle),
                    next_publish: blocked,
                    agent_changed: false,
                    process_exited: false,
                    stable_refresh_due: false,
                    now,
                },
                &mut pending_idle,
            ),
            DetectionTransitionDecision::PublishNext
        );
    }

    #[test]
    fn screen_publish_keeps_visible_working_without_pty_activity() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Idle, screen_detection(AgentState::Working), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                state: AgentState::Working,
                visible_idle: false,
                visible_blocker: false,
                visible_working: true,
                process_exited: false,
            }
        );
    }

    #[test]
    fn screen_publish_can_publish_idle_without_input_taint_delay() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Blocked, screen_detection(AgentState::Idle), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                state: AgentState::Idle,
                visible_idle: true,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
            }
        );
    }

    #[test]
    fn detection_content_change_tracks_raw_nonempty_reads_for_scan_scheduling() {
        let seq = AtomicU64::new(0);

        observe_detection_content_change(b"", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 0);

        observe_detection_content_change(b"\x1b[?2026h", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 1);

        observe_detection_content_change(b"body bytes", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn local_terminal_mutations_can_invalidate_idle_scan_skip() {
        let seq = AtomicU64::new(0);

        mark_detection_content_changed(&seq);

        assert_eq!(seq.load(Ordering::Relaxed), 1);
    }
}
