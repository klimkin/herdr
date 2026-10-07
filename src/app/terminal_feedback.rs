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
            revision: runtime.content_seq(),
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
