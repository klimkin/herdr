//! Bounded selected-terminal presentation after whole accepted input.

use super::*;
use crate::app::early_presentation::Work;

impl HeadlessServer {
    pub(super) fn cancel_terminal_source_opportunities(&mut self, client_id: u64) {
        if !self.app.early_presentation.terminal_enabled() || !self.app.early_presentation.pending()
        {
            return;
        }
        let targets: Vec<_> = self
            .clients
            .get(&client_id)
            .and_then(|client| client.render_state.last_pane_surface())
            .into_iter()
            .flat_map(|surface| &surface.panes)
            .filter_map(|pane| self.app.parse_pane_id(&pane.pane_id))
            .filter_map(|(workspace_index, pane_id)| {
                self.app
                    .state
                    .workspaces
                    .get(workspace_index)
                    .and_then(|workspace| workspace.terminal_id(pane_id))
                    .cloned()
            })
            .collect();
        for target in targets {
            let another_active_viewer =
                self.clients.iter().any(|(other_id, client)| {
                    *other_id != client_id
                        && client.is_active_shell_client()
                        && client
                            .render_state
                            .last_pane_surface()
                            .is_some_and(|surface| {
                                surface.panes.iter().any(|pane| {
                                    self.app
                                        .parse_pane_id(&pane.pane_id)
                                        .and_then(|(workspace_index, pane_id)| {
                                            self.app.state.workspaces.get(workspace_index).and_then(
                                                |workspace| workspace.terminal_id(pane_id),
                                            )
                                        })
                                        .is_some_and(|id| id == &target)
                                })
                            })
                });
            if !another_active_viewer {
                self.app.early_presentation.cancel_terminal(&target);
            }
        }
        self.app.prune_terminal_feedback();
    }

    pub(super) fn try_early_terminal_feedback(&mut self, now: Instant) -> bool {
        if !self.app.early_presentation.pending() {
            self.app.prune_terminal_feedback();
            return false;
        }
        self.app.prune_terminal_feedback();
        let clients = &self.clients;
        let runtimes = &self.app.terminal_runtimes;
        let signal = &self.app.render_dirty;
        let opportunity = self
            .app
            .early_presentation
            .oldest_ready(now, |opportunity| {
                let Work::Terminal {
                    terminal_id,
                    runtime_instance,
                    baseline_revision,
                    placement,
                } = &opportunity.work
                else {
                    return false;
                };
                let Some(runtime) = runtimes.get(terminal_id) else {
                    return false;
                };
                if runtime.runtime_instance() != *runtime_instance
                    || runtime.synchronized_output_active()
                {
                    return false;
                }
                let revision = runtime.content_seq();
                if !revision.is_multiple_of(2) || revision <= *baseline_revision {
                    return false;
                }
                signal.has_pending_source(placement.pane_id)
                    && clients.values().any(|client| {
                        client.is_active_shell_client()
                            && client
                                .render_state
                                .last_pane_surface()
                                .is_some_and(|surface| {
                                    surface
                                        .panes
                                        .iter()
                                        .any(|pane| pane.pane_id == placement.public_pane_id)
                                })
                    })
            });
        let Some(opportunity) = opportunity else {
            return false;
        };
        let Work::Terminal {
            runtime_instance,
            placement,
            ..
        } = &opportunity.work
        else {
            return false;
        };
        let source = placement.pane_id;
        if !self
            .app
            .early_presentation
            .admit(opportunity.id, now, false)
        {
            return false;
        }
        let presentation = crate::latency_prof::PresentationTrace::selected(now, now);
        #[cfg(feature = "latency-prof")]
        crate::latency_prof::record_context_at(
            "opportunity.early_terminal_admitted",
            opportunity.id,
            0,
            0,
            crate::latency_prof::now(),
            presentation.context(),
        );
        let mut pending = self.app.render_dirty.take_pending();
        if !pending.request.pty_sources.contains(&source) {
            return false;
        }
        if self.app.experiments.presentation
            == crate::latency_experiments::PresentationPolicy::TargetAll
        {
            let material = self.prepare_target_pane_surface_traced(
                source,
                *runtime_instance,
                presentation.context(),
            );
            if material != super::retained_surface::RetainedOutcome::Ready {
                drop(pending);
                self.app.prune_terminal_feedback();
                return false;
            }
            let (_, outer_title_synced) =
                self.sync_terminal_title_sources(&pending.request.terminal_title_sources);
            if !outer_title_synced {
                self.sync_window_title();
            }
            let outcome = self.render_all_dirty_terminal_feedback(presentation.context());
            if outcome.ordinary_work_handled {
                pending.complete();
                self.app.prune_terminal_feedback();
                return true;
            }
            self.app.full_redraw_pending = true;
            drop(pending);
            self.app.prune_terminal_feedback();
            return false;
        }
        if self.render_target_pane_surface_traced(source, *runtime_instance, presentation.context())
            == super::retained_surface::RetainedOutcome::Handled
        {
            pending.request.pty_sources.remove(&source);
        }
        // Restore unrelated work without changing ordinary clocks or waking.
        drop(pending);
        self.app.prune_terminal_feedback();
        false
    }
}

impl HeadlessServer {
    pub(super) fn has_ready_feedback_recovery(&self) -> bool {
        self.feedback_recovery
            .iter()
            .any(|id| self.feedback_recovery_ready(*id))
    }

    pub(super) fn feedback_recovery_ready(&self, client_id: u64) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        if !client.is_active_shell_client() || client.writer.is_none() {
            return false;
        }
        let Some(target) = self.shell_target_for_client(client_id) else {
            return true;
        };
        let Some(tab) = self
            .app
            .state
            .workspaces
            .get(target.workspace_index)
            .and_then(|workspace| workspace.tabs.get(target.tab_index))
        else {
            return true;
        };
        if self.popup_owner_tab_id == self.shell_tab_id_for_client(client_id)
            && self
                .app
                .state
                .popup_pane
                .as_ref()
                .and_then(|popup| self.app.terminal_runtimes.get(&popup.terminal_id))
                .is_some_and(|runtime| runtime.synchronized_output_active())
        {
            return false;
        }
        // A synchronized producer owns the next wake; do not poll its transaction.
        !tab.panes
            .keys()
            .filter(|pane| !tab.zoomed || **pane == tab.layout.focused())
            .any(|pane| {
                self.app
                    .state
                    .runtime_for_pane_in_workspace(
                        &self.app.terminal_runtimes,
                        target.workspace_index,
                        *pane,
                    )
                    .is_some_and(|runtime| runtime.synchronized_output_active())
            })
    }

    /// Queue delivery reuses the original admitted construction and baseline.
    pub(super) fn retry_pending_feedback(&mut self, client_id: u64) -> bool {
        let valid = self.clients.get(&client_id).is_some_and(|client| {
            client
                .pending_feedback
                .as_ref()
                .is_some_and(|pending| pending.capture.valid(&self.app, client))
        });
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let Some(mut pending) = client.pending_feedback.take() else {
            return false;
        };
        if !valid || self.handoff_in_progress {
            client.request_recompute();
            client.defer_full_render();
            client.feedback_recovery = true;
            self.feedback_recovery.insert(client_id);
            return false;
        }
        let Some(writer) = client.writer.clone() else {
            client.request_recompute();
            client.defer_full_render();
            client.feedback_recovery = true;
            self.feedback_recovery.insert(client_id);
            return false;
        };
        let bytes = std::mem::take(&mut pending.bytes);
        match writer
            .render
            .send_admitted_feedback_traced(bytes, &pending.trace)
        {
            Ok(()) => {
                let followup = client.render_pending || pending.capture.newer_output(&self.app);
                let context = crate::latency_prof::presentation::primary_context(&pending.trace);
                client.render_state.commit_sent_frame(pending.prepared);
                client.render_pending = followup;
                pending
                    .receipts
                    .acknowledge(&mut self.app, client_id, context);
                if followup {
                    client.feedback_recovery = true;
                    self.feedback_recovery.insert(client_id);
                }
                false
            }
            Err(std::sync::mpsc::TrySendError::Full(bytes)) => {
                pending.bytes = bytes;
                client.pending_feedback = Some(pending);
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.remove_client_and_resize_if_needed(client_id);
                false
            }
        }
    }
}
