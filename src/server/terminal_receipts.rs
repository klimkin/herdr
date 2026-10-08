//! Construction-time runtime identities, kept outside frozen surface codecs.

use crate::{app::App, protocol, terminal::TerminalId};

#[derive(Clone, Default)]
pub(super) struct TerminalReceipts(Vec<TargetEnqueue>);

#[derive(Clone)]
pub(super) struct TargetEnqueue {
    pane_id: String,
    terminal_id: TerminalId,
    runtime_instance: u64,
    revision: u64,
    attempt: Option<crate::app::early_presentation::TerminalAttempt>,
}

impl TerminalReceipts {
    pub(super) fn capture(
        &mut self,
        app: &App,
        workspace_index: usize,
        pane_id: crate::layout::PaneId,
        revision: u64,
    ) {
        if !app.early_presentation.terminal_enabled()
            || !app.early_presentation.pending()
            || revision == 0
            || !revision.is_multiple_of(2)
        {
            return;
        }
        let Some(terminal_id) = app
            .state
            .workspaces
            .get(workspace_index)
            .and_then(|workspace| workspace.terminal_id(pane_id))
        else {
            return;
        };
        if !app.early_presentation.tracks_terminal(terminal_id) {
            return;
        }
        let Some(runtime) = app.terminal_runtimes.get(terminal_id) else {
            return;
        };
        let Some(public_pane_id) = app.public_pane_id(workspace_index, pane_id) else {
            return;
        };
        self.0.push(TargetEnqueue {
            pane_id: public_pane_id,
            terminal_id: terminal_id.clone(),
            runtime_instance: runtime.runtime_instance(),
            revision,
            attempt: app.early_presentation.terminal_attempt(terminal_id),
        });
    }

    /// A saved recipient cannot suppress another compatible viewer's urgency.
    pub(super) fn has_ready_peer(
        &self,
        app: &App,
        clients: &std::collections::HashMap<u64, super::clients::ClientConnection>,
        excluded: u64,
    ) -> bool {
        clients.iter().any(|(id, client)| {
            *id != excluded
                && client.is_active_shell_client()
                && client.writer.is_some()
                && client.deferred_render() == super::clients::DeferredRender::None
                && !client.render_state.requires_recompute()
                && client
                    .render_state
                    .last_pane_surface()
                    .is_some_and(|surface| {
                        surface.projection_revision == client.shell_projection_revision
                            && surface.frame.width == client.terminal_size.0
                            && surface.frame.height == client.terminal_size.1
                            && surface.popup.is_none()
                            && surface.graphics.assets.is_empty()
                            && surface.graphics.placements.is_empty()
                            && surface.graphics.retained_assets.is_empty()
                            && surface.frame.graphics.is_empty()
                            && self.0.iter().any(|receipt| {
                                app.terminal_runtimes.get(&receipt.terminal_id).is_some_and(
                                    |runtime| {
                                        runtime.runtime_instance() == receipt.runtime_instance
                                            && !runtime.synchronized_output_active()
                                            && !runtime.graphics_may_have_placements()
                                    },
                                ) && surface
                                    .panes
                                    .iter()
                                    .any(|pane| pane.pane_id == receipt.pane_id)
                            })
                    })
        })
    }

    pub(super) fn storage(&self) -> Option<usize> {
        let mut bytes = self
            .0
            .capacity()
            .checked_mul(std::mem::size_of::<TargetEnqueue>())?;
        for receipt in &self.0 {
            bytes = bytes
                .checked_add(receipt.pane_id.capacity())?
                .checked_add(receipt.terminal_id.as_str().len())?;
        }
        Some(bytes)
    }

    /// Comparator construction may follow a material probe with a newer PTY
    /// snapshot. A revision alone cannot certify changed target presentation.
    pub(super) fn retain_material(
        &mut self,
        baseline: Option<&protocol::PaneSurfaceFrame>,
        candidate: &protocol::PaneSurfaceFrame,
        context: crate::latency_prof::TraceContext,
    ) {
        self.0.retain(|receipt| {
            let next = candidate
                .panes
                .iter()
                .find(|pane| pane.pane_id == receipt.pane_id);
            let old = baseline.and_then(|surface| {
                surface
                    .panes
                    .iter()
                    .find(|pane| pane.pane_id == receipt.pane_id)
            });
            let material = match (baseline, old, next) {
                (Some(surface), Some(old), Some(next))
                    if old.inner_rect == next.inner_rect
                        && surface.frame.width == candidate.frame.width
                        && surface.frame.height == candidate.frame.height =>
                {
                    let rect = next.inner_rect;
                    let width = usize::from(candidate.frame.width);
                    let rows_changed = (rect.y..rect.y.saturating_add(rect.height)).any(|y| {
                        let start = usize::from(y) * width + usize::from(rect.x);
                        let end = start + usize::from(rect.width);
                        match (
                            surface.frame.cells.get(start..end),
                            candidate.frame.cells.get(start..end),
                        ) {
                            (Some(before), Some(after)) => before != after,
                            _ => false,
                        }
                    });
                    rows_changed
                        || (next.focused && surface.frame.cursor != candidate.frame.cursor)
                        || old.mouse_reporting != next.mouse_reporting
                        || old.sgr_pixel_mouse != next.sgr_pixel_mouse
                        || old.alternate_screen_active != next.alternate_screen_active
                        || old.scroll != next.scroll
                }
                _ => false,
            };
            #[cfg(feature = "latency-prof")]
            if !material && crate::latency_prof::active() {
                crate::latency_prof::record_context_at(
                    "server.target_material_absent",
                    crate::latency_prof::bytes_id(receipt.pane_id.as_bytes()),
                    receipt.revision,
                    0,
                    crate::latency_prof::now(),
                    crate::latency_prof::TraceContext {
                        runtime_instance: receipt.runtime_instance,
                        ..context
                    },
                );
            }
            #[cfg(not(feature = "latency-prof"))]
            let _ = context;
            material
        });
    }

    pub(super) fn retain_incorporated(&mut self, panes: Option<&[protocol::PaneSurfacePane]>) {
        self.0.retain(|receipt| {
            panes.is_some_and(|panes| {
                panes.iter().any(|pane| {
                    pane.pane_id == receipt.pane_id && pane.content_revision == receipt.revision
                })
            })
        });
    }

    /// A committed surface must still name the captured exact revision. Never
    /// obtain a newer revision or replacement identity after queue admission.
    pub(super) fn acknowledge(
        &self,
        app: &mut App,
        client_id: u64,
        context: crate::latency_prof::TraceContext,
        recipient_ready: bool,
    ) {
        for receipt in &self.0 {
            if app
                .terminal_runtimes
                .get(&receipt.terminal_id)
                .is_none_or(|runtime| runtime.runtime_instance() != receipt.runtime_instance)
            {
                continue;
            }
            #[cfg(feature = "latency-prof")]
            if crate::latency_prof::active() {
                crate::latency_prof::record_context_at(
                    "server.target_enqueued",
                    crate::latency_prof::bytes_id(receipt.pane_id.as_bytes()),
                    receipt.revision,
                    client_id,
                    crate::latency_prof::now(),
                    crate::latency_prof::TraceContext {
                        runtime_instance: receipt.runtime_instance,
                        ..context
                    },
                );
            }
            if let Some(mut advance) = app.early_presentation.acknowledge_terminal(
                &receipt.terminal_id,
                receipt.runtime_instance,
                receipt.revision,
                client_id,
                context,
                receipt.attempt,
            ) {
                advance.wake_allowed &= recipient_ready;
                if app.render_dirty.advance_target_ready(&advance) {
                    app.render_notify.notify_one();
                }
            }
        }
        app.prune_terminal_feedback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::early_presentation::{EarlyPresentation, TerminalPlacement};
    use crate::latency_experiments::PresentationPolicy;

    #[tokio::test]
    async fn ordinary_enqueue_preserves_feedback_and_wakes_newer_pending_output() {
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("feedback"));
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane)
            .expect("test terminal")
            .clone();
        let (runtime, _receiver) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("test runtime");
        let instance = runtime.runtime_instance();
        let now = std::time::Instant::now();
        app.early_presentation = EarlyPresentation::new(PresentationPolicy::Target);
        let opportunity = app
            .early_presentation
            .accepted_terminal(
                terminal_id.clone(),
                TerminalPlacement {
                    workspace_id: app.state.workspaces[0].id.clone(),
                    tab_root: pane,
                    pane_id: pane,
                    public_pane_id: "w1:p1".into(),
                },
                instance,
                20,
                None,
                now,
            )
            .expect("accepted test input");
        app.render_dirty.arm_target_ready(
            pane,
            instance,
            20,
            opportunity.expires_at,
            opportunity.id,
        );
        app.render_dirty.request_pty_ready(pane, instance, 22);
        app.render_dirty.request_pty_ready(pane, instance, 24);
        app.render_dirty.take_pending().complete();
        assert!(!app.render_dirty.is_pending());
        let receipt = TerminalReceipts(vec![TargetEnqueue {
            pane_id: "w1:p1".into(),
            terminal_id,
            runtime_instance: instance,
            revision: 22,
            attempt: None,
        }]);
        receipt.acknowledge(
            &mut app,
            4,
            crate::latency_prof::TraceContext::default(),
            true,
        );
        assert!(app.render_dirty.has_pending_source(pane));
        assert!(app.early_presentation.contains(opportunity.id));
        assert!(app.early_presentation.admit(
            opportunity.id,
            now + std::time::Duration::from_millis(1),
            false
        ));
    }
}
