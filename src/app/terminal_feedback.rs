//! Accepted interactive runtime input, shared by API and terminal clients.

use super::early_presentation::{OriginLease, Work};
use crate::layout::PaneId;
use crate::terminal::TerminalId;

pub(crate) struct TerminalInputBaseline {
    terminal_id: TerminalId,
    placement: super::early_presentation::TerminalPlacement,
    pane_id: PaneId,
    instance: u64,
    revision: u64,
}

fn input_watermark(revision: u64) -> Option<u64> {
    // Odd revisions announce an update already in progress. Exclude its
    // completion from input feedback without waiting for the terminal writer.
    revision.checked_add(revision & 1)
}

impl super::App {
    /// Disabled experiments exit before identity cloning, clocks or revisions.
    pub(crate) fn terminal_input_baseline(
        &self,
        workspace_index: usize,
        pane_id: PaneId,
    ) -> Option<TerminalInputBaseline> {
        if !self.early_presentation.terminal_enabled() {
            return None;
        }
        let terminal_id = self.state.terminal_id_for_pane(workspace_index, pane_id)?;
        let runtime = self.terminal_runtimes.get(&terminal_id)?;
        let revision = input_watermark(runtime.content_seq())?;
        let workspace = self.state.workspaces.get(workspace_index)?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| tab.panes.contains_key(&pane_id))?;
        let placement = super::early_presentation::TerminalPlacement {
            workspace_id: workspace.id.clone(),
            tab_root: tab.root_pane,
            pane_id,
            public_pane_id: self.public_pane_id(workspace_index, pane_id)?,
        };
        Some(TerminalInputBaseline {
            terminal_id,
            placement,
            pane_id,
            instance: runtime.runtime_instance(),
            revision,
        })
    }

    pub(crate) fn accepted_terminal_input(
        &mut self,
        baseline: Option<TerminalInputBaseline>,
        accepted: bool,
        origin: Option<OriginLease>,
    ) {
        if self.early_presentation.terminal_enabled() {
            self.terminal_input_outcome(
                if accepted {
                    "opportunity.input_accepted"
                } else {
                    "opportunity.input_empty"
                },
                u64::from(accepted),
            );
        }
        let Some(baseline) = baseline.filter(|_| accepted) else {
            return;
        };
        let Some(opportunity) = self.early_presentation.accepted_terminal(
            baseline.terminal_id,
            baseline.placement,
            baseline.instance,
            baseline.revision,
            origin,
            std::time::Instant::now(),
        ) else {
            self.terminal_input_outcome("opportunity.input_no_baseline", baseline.revision);
            return;
        };
        #[cfg(feature = "latency-prof")]
        if crate::latency_prof::active() {
            crate::latency_prof::event::current().record("opportunity.input", opportunity.id);
        }
        let Work::Terminal {
            runtime_instance,
            baseline_revision,
            ..
        } = opportunity.work
        else {
            return;
        };
        self.render_dirty.arm_target_ready(
            baseline.pane_id,
            runtime_instance,
            baseline_revision,
            opportunity.expires_at,
            opportunity.id,
        );
        self.prune_terminal_feedback();
    }

    pub(crate) fn rejected_terminal_input(&self) {
        if self.early_presentation.terminal_enabled() {
            self.terminal_input_outcome("opportunity.input_rejected", 0);
        }
    }

    fn terminal_input_outcome(&self, stage: &'static str, value: u64) {
        #[cfg(feature = "latency-prof")]
        if crate::latency_prof::active() {
            crate::latency_prof::event::current().record(stage, value);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (stage, value);
    }

    pub(crate) fn prune_terminal_feedback(&mut self) {
        if !self.early_presentation.terminal_enabled() {
            return;
        }
        let state = &self.state;
        let runtimes = &self.terminal_runtimes;
        self.early_presentation
            .prune_terminals(std::time::Instant::now(), |work| {
                let Work::Terminal {
                    terminal_id,
                    runtime_instance,
                    placement,
                    ..
                } = work
                else {
                    return true;
                };
                runtimes
                    .get(terminal_id)
                    .is_some_and(|runtime| runtime.runtime_instance() == *runtime_instance)
                    && state
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == placement.workspace_id)
                        .and_then(|workspace| {
                            workspace
                                .tabs
                                .iter()
                                .find(|tab| tab.root_pane == placement.tab_root)
                        })
                        .and_then(|tab| tab.terminal_id(placement.pane_id))
                        == Some(terminal_id)
            });
        self.render_dirty
            .prune_target_ready(|id| self.early_presentation.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::early_presentation::{EarlyPresentation, Opportunity};
    use crate::latency_experiments::PresentationPolicy;
    use std::time::Instant;

    #[test]
    fn input_watermark_excludes_in_progress_updates_without_wrapping() {
        for revision in [0, 2, 20, u64::MAX - 1] {
            assert_eq!(input_watermark(revision), Some(revision));
        }
        for revision in [1, 21, u64::MAX - 2] {
            assert_eq!(input_watermark(revision), Some(revision + 1));
        }
        assert_eq!(input_watermark(u64::MAX), None);
    }

    fn ready(app: &mut super::super::App, revision: u64) -> Option<Opportunity> {
        app.early_presentation.oldest_ready(Instant::now(), |opportunity| {
            matches!(opportunity.work, Work::Terminal { baseline_revision, .. } if revision > baseline_revision)
        })
    }

    #[tokio::test]
    async fn input_during_terminal_update_preserves_later_feedback() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.early_presentation = EarlyPresentation::new(PresentationPolicy::ActionFullTarget);
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("input-feedback")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let terminal = app.state.terminal_id_for_pane(0, pane).unwrap();
        let (runtime, _input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal.clone(), runtime);

        let runtime = app.terminal_runtimes.get(&terminal).unwrap();
        runtime.test_process_pty_bytes(b"previous");
        let previous = runtime.content_seq();
        let baseline = runtime.test_process_pty_bytes_with_hook(b"older output", || {
            assert_eq!(runtime.content_seq(), previous + 1);
            app.terminal_input_baseline(0, pane)
                .expect("input baseline")
        });
        let completed = runtime.content_seq();
        let instance = runtime.runtime_instance();
        app.accepted_terminal_input(Some(baseline), true, None);
        assert!(app.early_presentation.tracks_terminal(&terminal));
        assert!(
            ready(&mut app, completed).is_none(),
            "older update cannot satisfy input"
        );

        // Bulk output already owns this source; only a qualifying target can wake again.
        app.render_dirty.request_pty(pane);
        assert!(!app
            .render_dirty
            .request_pty_ready(pane, instance, completed));
        app.terminal_runtimes
            .get(&terminal)
            .unwrap()
            .test_process_pty_bytes(b"echo");
        let echo_revision = app.terminal_runtimes.get(&terminal).unwrap().content_seq();
        assert!(app
            .render_dirty
            .request_pty_ready(pane, instance, echo_revision));
        let opportunity = ready(&mut app, echo_revision).expect("echo presentation opportunity");
        assert!(
            matches!(opportunity.work, Work::Terminal { baseline_revision, .. } if baseline_revision == completed)
        );
        assert!(app
            .early_presentation
            .admit(opportunity.id, Instant::now(), false));
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }
}
