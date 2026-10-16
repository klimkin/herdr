use super::*;

const MAX_PREVIEWS: usize = 8;
const MAX_PREVIEW_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewIdentity {
    endpoint_id: ClientEndpointId,
    generation: Option<u64>,
    boot_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewFocus {
    workspace: Option<String>,
    tab: Option<String>,
    pane: Option<String>,
}

impl PreviewFocus {
    fn from_snapshot(snapshot: &ClientShellSnapshot) -> Self {
        Self {
            workspace: snapshot.focused_workspace_id.clone(),
            tab: snapshot.focused_tab_id.clone(),
            pane: snapshot.focused_pane_id.clone(),
        }
    }

    fn matches(
        &self,
        target: Option<&ClientEndpointFocusTarget>,
        snapshot: &ClientShellSnapshot,
    ) -> bool {
        let valid = self.workspace.as_ref().is_none_or(|id| {
            snapshot
                .workspaces
                .iter()
                .any(|workspace| &workspace.workspace_id == id)
        }) && self.tab.as_ref().is_none_or(|id| {
            snapshot
                .tabs
                .iter()
                .any(|tab| &tab.tab_id == id && Some(&tab.workspace_id) == self.workspace.as_ref())
        }) && self.pane.as_ref().is_none_or(|id| {
            snapshot.panes.iter().any(|pane| {
                &pane.pane_id == id
                    && Some(&pane.tab_id) == self.tab.as_ref()
                    && Some(&pane.workspace_id) == self.workspace.as_ref()
            })
        });
        valid
            && match target {
                None => *self == Self::from_snapshot(snapshot),
                Some(ClientEndpointFocusTarget::Workspace(id)) => {
                    self.workspace.as_ref() == Some(id)
                        && snapshot.workspaces.iter().any(|workspace| {
                            &workspace.workspace_id == id
                                && Some(&workspace.active_tab_id) == self.tab.as_ref()
                        })
                }
                Some(ClientEndpointFocusTarget::Tab(id)) => self.tab.as_ref() == Some(id),
                Some(ClientEndpointFocusTarget::Pane(id)) => self.pane.as_ref() == Some(id),
                #[cfg(windows)]
                Some(ClientEndpointFocusTarget::Notification { pane_id, .. }) => {
                    self.pane.as_ref() == Some(pane_id)
                }
            }
    }
}

struct RetainedTextPreview {
    identity: PreviewIdentity,
    focus: PreviewFocus,
    host_size: (u16, u16),
    frame: FrameData,
    bytes: usize,
}

pub(super) struct PendingPreviewSelection {
    identity: PreviewIdentity,
    target: Option<ClientEndpointFocusTarget>,
}

#[derive(Default)]
pub(super) struct TextPreviews {
    entries: VecDeque<RetainedTextPreview>,
    pub(super) pending: Option<PendingPreviewSelection>,
}

impl TextPreviews {
    pub(super) fn retain_endpoints(&mut self, endpoints: &[ClientShellEndpoint]) {
        self.entries.retain(|entry| {
            endpoints.iter().any(|endpoint| {
                endpoint.endpoint_id == entry.identity.endpoint_id
                    && endpoint.status != ClientEndpointStatus::Disabled
            })
        });
    }

    fn insert(&mut self, entry: RetainedTextPreview) {
        self.entries.retain(|previous| {
            previous.identity != entry.identity
                || previous.focus != entry.focus
                || previous.host_size != entry.host_size
        });
        if entry.bytes > MAX_PREVIEW_BYTES {
            return;
        }
        let mut bytes = self.entries.iter().fold(entry.bytes, |total, previous| {
            total.saturating_add(previous.bytes)
        });
        while self.entries.len() >= MAX_PREVIEWS || bytes > MAX_PREVIEW_BYTES {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            bytes = bytes.saturating_sub(removed.bytes);
        }
        self.entries.push_back(entry);
    }
}

impl ClientShellState {
    pub(crate) fn remember_text_preview(&mut self, cols: u16, rows: u16) {
        if self.previews.pending.is_some() || self.pending_pane_surface.is_some() {
            return;
        }
        let (Some(snapshot), Some(surface)) =
            (self.snapshot.as_deref(), self.pane_surface.as_ref())
        else {
            return;
        };
        let size = self.surface_size(cols, rows);
        if self.active_snapshot_generation != self.pane_surface_generation
            || snapshot.boot_id != surface.boot_id
            || snapshot.revision != surface.projection_revision
            || (surface.frame.width, surface.frame.height) != (size.cols, size.rows)
        {
            return;
        }
        let identity = PreviewIdentity {
            endpoint_id: self.active_endpoint_id.clone(),
            generation: self.active_snapshot_generation,
            boot_id: snapshot.boot_id.clone(),
        };
        let focus = PreviewFocus::from_snapshot(snapshot);
        let frame = FrameData {
            cells: surface
                .frame
                .cells
                .iter()
                .map(|cell| {
                    let mut cell = cell.clone();
                    cell.hyperlink = None;
                    cell
                })
                .collect(),
            width: surface.frame.width,
            height: surface.frame.height,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let bytes = frame.cells.iter().fold(
            frame
                .cells
                .capacity()
                .saturating_mul(std::mem::size_of::<crate::protocol::CellData>())
                .saturating_add(std::mem::size_of::<RetainedTextPreview>())
                .saturating_add(identity.boot_id.capacity()),
            |total, cell| total.saturating_add(cell.symbol.capacity()),
        );
        let bytes = [&focus.workspace, &focus.tab, &focus.pane]
            .into_iter()
            .flatten()
            .fold(bytes, |total, id| total.saturating_add(id.capacity()));
        self.previews.insert(RetainedTextPreview {
            identity,
            focus,
            host_size: (cols, rows),
            frame,
            bytes,
        });
    }

    pub(crate) fn select_pending_preview(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        target: Option<ClientEndpointFocusTarget>,
    ) {
        let boot_id = self
            .endpoints
            .iter()
            .find(|endpoint| {
                &endpoint.endpoint_id == endpoint_id && endpoint.snapshot_generation == generation
            })
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .map(|snapshot| snapshot.boot_id.clone())
            .unwrap_or_default();
        self.previews.pending = Some(PendingPreviewSelection {
            identity: PreviewIdentity {
                endpoint_id: endpoint_id.clone(),
                generation,
                boot_id,
            },
            target,
        });
        self.reveal_navigation_workspace = true;
        self.clear_link_hover();
        self.selection = None;
        self.word_selection_gesture = None;
        self.pane_mouse_gesture = None;
        self.stop_selection_autoscroll();
        self.copy_mode = None;
        self.reset_copy_pipeline();
        if self.mode == ClientShellMode::Copy {
            self.mode = ClientShellMode::Terminal;
        }
        if matches!(
            self.chrome_drag,
            Some(
                ClientChromeDrag::PaneSplit { .. }
                    | ClientChromeDrag::PaneScrollbar { .. }
                    | ClientChromeDrag::Tab { .. }
            )
        ) {
            self.chrome_drag = None;
        }
    }

    pub(crate) fn clear_pending_preview(&mut self) {
        self.previews.pending = None;
    }

    pub(crate) fn has_pending_preview(&self) -> bool {
        self.previews.pending.is_some()
    }

    pub(crate) fn finish_coherent_preview(&mut self) {
        let Some(pending) = self.previews.pending.as_ref() else {
            return;
        };
        let coherent = self
            .snapshot
            .as_deref()
            .zip(self.pane_surface.as_ref())
            .is_some_and(|(snapshot, surface)| {
                pending.identity.endpoint_id == self.active_endpoint_id
                    && pending.identity.generation == self.active_snapshot_generation
                    && self.active_snapshot_generation == self.pane_surface_generation
                    && pending.identity.boot_id == snapshot.boot_id
                    && surface.boot_id == snapshot.boot_id
                    && surface.projection_revision == snapshot.revision
                    && PreviewFocus::from_snapshot(snapshot)
                        .matches(pending.target.as_ref(), snapshot)
            });
        if coherent {
            self.clear_pending_preview();
        }
    }

    pub(crate) fn compose_pending_preview(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Option<crate::client::frame_output::ComposedFrame> {
        let pending = self.previews.pending.as_ref()?;
        let endpoint_id = pending.identity.endpoint_id.clone();
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot.as_deref();
        let live = endpoint.status == ClientEndpointStatus::Online
            && endpoint.snapshot_generation == pending.identity.generation
            && snapshot.is_some_and(|snapshot| snapshot.boot_id == pending.identity.boot_id);
        let workspace_id = live
            .then(|| {
                snapshot.and_then(|snapshot| match pending.target.as_ref() {
                    Some(ClientEndpointFocusTarget::Workspace(id)) => Some(id.as_str()),
                    Some(ClientEndpointFocusTarget::Tab(id)) => snapshot
                        .tabs
                        .iter()
                        .find(|tab| &tab.tab_id == id)
                        .map(|tab| tab.workspace_id.as_str()),
                    Some(ClientEndpointFocusTarget::Pane(id)) => snapshot
                        .panes
                        .iter()
                        .find(|pane| &pane.pane_id == id)
                        .map(|pane| pane.workspace_id.as_str()),
                    #[cfg(windows)]
                    Some(ClientEndpointFocusTarget::Notification { pane_id, .. }) => snapshot
                        .panes
                        .iter()
                        .find(|pane| &pane.pane_id == pane_id)
                        .map(|pane| pane.workspace_id.as_str()),
                    None => snapshot.focused_workspace_id.as_deref(),
                })
            })
            .flatten();
        let selected = workspace_id
            .and_then(|id| self.navigation_target(&endpoint_id, id))
            .filter(|target| self.navigation_target_valid(target));
        let pane_id = live
            .then_some(match pending.target.as_ref() {
                Some(ClientEndpointFocusTarget::Pane(id)) => Some(id.as_str()),
                #[cfg(windows)]
                Some(ClientEndpointFocusTarget::Notification { pane_id, .. }) => {
                    Some(pane_id.as_str())
                }
                _ => None,
            })
            .flatten()
            .filter(|id| {
                snapshot
                    .is_some_and(|snapshot| snapshot.panes.iter().any(|pane| pane.pane_id == *id))
            });
        let layout = self.layout(cols, rows);
        let area = Rect::new(0, 0, cols, rows);
        let mut buffer = self
            .composition_buffer
            .take()
            .unwrap_or_else(|| Buffer::empty(area));
        buffer.resize(area);
        buffer.reset();
        buffer.set_style(
            area,
            Style::default()
                .fg(self.config.palette.text)
                .bg(self.config.palette.panel_bg),
        );
        self.hits = ShellHitMap::default();
        let mut render_state = render::ShellRenderState {
            machine_diagnostics: &self.machine_diagnostics,
            endpoints: &self.endpoints,
            active_endpoint_id: &endpoint_id,
            collapsed_endpoints: &self.collapsed_endpoints,
            collapsed_groups: &self.collapsed_groups,
            remote_collapsed_groups: &self.remote_collapsed_groups,
            workspace_scroll: &mut self.workspace_scroll,
            agent_scroll: &mut self.agent_scroll,
            tab_scroll: &mut self.tab_scroll,
            reveal_focused_workspace: &mut self.reveal_focused_workspace,
            reveal_focused_tab: &mut self.reveal_focused_tab,
            sidebar_collapsed: self.sidebar_collapsed,
            sidebar_section_split: self.sidebar_section_split,
            tab_drag_insert_index: None,
            selected_workspace_id: self
                .navigate_workspace_id
                .as_ref()
                .filter(|_| self.mode == ClientShellMode::Navigate)
                .or(selected.as_ref()),
            selected_agent_id: pane_id.map(|id| (&endpoint_id, id)),
            reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
            dragged_workspace_id: None,
            workspace_drop_indicator_row: None,
        };
        if !layout.sidebar.is_empty() {
            if self.sidebar_collapsed {
                super::endpoint_sidebar::render_collapsed(
                    &mut buffer,
                    layout.sidebar,
                    &self.config,
                    &mut render_state,
                    &mut self.hits,
                );
            } else {
                super::endpoint_sidebar::render_expanded(
                    &mut buffer,
                    layout.sidebar,
                    snapshot,
                    &self.config,
                    &mut render_state,
                    &mut self.hits,
                );
            }
        }
        self.hits.new_workspace = Rect::default();
        self.hits.global_launcher = Rect::default();
        if !self.config.mouse_capture {
            self.hits = ShellHitMap::default();
        }
        let retained = snapshot.filter(|_| live).and_then(|snapshot| {
            self.previews.entries.iter().rev().find(|entry| {
                entry.identity == pending.identity
                    && entry.host_size == (cols, rows)
                    && entry.frame.width == layout.pane_surface.width
                    && entry.frame.height == layout.pane_surface.height
                    && entry.focus.matches(pending.target.as_ref(), snapshot)
            })
        });
        let mut frame = if let Some(mut frame) = self.composition_frame.take() {
            frame.replace_from_ratatui_buffer(&buffer, None);
            frame
        } else {
            FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
        };
        if let Some(retained) = retained {
            blit_pane_surface(&mut frame, &retained.frame, layout.pane_surface);
        }
        let message = format!("{} · syncing", endpoint.label);
        let message_area = if !layout.tab_bar.is_empty() {
            layout.tab_bar
        } else {
            Rect::new(
                layout.pane_surface.x,
                layout.pane_surface.y,
                layout.pane_surface.width,
                1,
            )
        };
        frame.write_ratatui_buffer(&mut buffer);
        render::put_text(
            &mut buffer,
            message_area.x,
            message_area.y,
            message_area.width,
            &message,
            Style::default()
                .fg(self.config.palette.overlay0)
                .bg(self.config.palette.panel_bg),
        );
        render::render_mode_bar(
            &mut buffer,
            layout.pane_surface,
            self.mode,
            None,
            self.endpoint_error.as_deref(),
            false,
            &self.config.keybinds,
            &self.config.palette,
        );
        if let Some(snapshot) = snapshot {
            if !layout.mobile_header.is_empty() {
                super::mobile::render_mobile_header(
                    &mut buffer,
                    layout.mobile_header,
                    snapshot,
                    &self.config,
                    &mut self.hits,
                );
                if self.mode == ClientShellMode::Navigate && self.overlay.is_none() {
                    super::mobile::render_mobile_switcher(
                        &mut buffer,
                        area,
                        snapshot,
                        &self.endpoints,
                        &endpoint_id,
                        &self.config,
                        self.navigate_workspace_id.as_ref().or(selected.as_ref()),
                        &mut self.mobile_switcher_scroll,
                        &mut self.reveal_mobile_workspace,
                        &mut self.hits,
                    );
                    self.hits.mobile_targets.retain(|(_, target)| {
                        matches!(
                            target,
                            ClientMobileTarget::Machine(_)
                                | ClientMobileTarget::Workspace { .. }
                                | ClientMobileTarget::Agent { .. }
                        )
                    });
                }
            }
            if let Some(overlay) = self.overlay.as_ref() {
                match overlay {
                    ClientShellOverlay::ContextMenu(menu) => {
                        let rendered =
                            render::render_context_menu(&mut buffer, menu, &self.config.palette)?;
                        self.hits.context_menu_rows = rendered.menu_rows;
                    }
                    ClientShellOverlay::GlobalMenu(menu) => {
                        let rendered = render::render_global_menu(
                            &mut buffer,
                            self.hits.global_launcher,
                            menu,
                            snapshot,
                            &self.config.palette,
                        )?;
                        self.hits.global_menu_rows = rendered.menu_rows;
                    }
                    _ => {
                        let rendered = render::render_client_overlay(
                            &mut buffer,
                            overlay,
                            snapshot,
                            &self.endpoints,
                            &endpoint_id,
                            &self.config.keybinds,
                            &self.config.palette,
                        )?;
                        self.hits.apply_overlay(rendered);
                    }
                }
            }
        }
        if let Some(notice) = &self.visible_endpoint_notice {
            self.hits.notification_toast =
                endpoint_notices::render_notice(&mut buffer, area, notice, 1, &self.config.palette);
        }
        frame.replace_from_ratatui_buffer(&buffer, None);
        self.composition_buffer = Some(buffer);
        self.last_composed_at = Some(std::time::Instant::now());
        self.last_composed_size = Some((cols, rows));
        self.retained_rename = None;
        Some(crate::client::frame_output::ComposedFrame::preview(frame))
    }
}

#[cfg(test)]
#[path = "tests/preview.rs"]
mod tests;
