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
        });
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
            app.early_presentation.acknowledge_terminal(
                &receipt.terminal_id,
                receipt.runtime_instance,
                receipt.revision,
                client_id,
                context,
            );
        }
        app.prune_terminal_feedback();
    }
}
