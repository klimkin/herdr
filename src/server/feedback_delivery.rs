//! Immutable, already-admitted text delivery waiting for one recipient queue.

use super::{
    clients::ClientConnection, render_stream::PreparedRender, terminal_receipts::TerminalReceipts,
};
use crate::{app::App, protocol, terminal::TerminalId};

// Independently bound metadata, including instrumentation and receipt ownership.
const MAX_CAPTURED_PANES: usize = 256;

struct CapturedRuntime {
    pane_id: String,
    terminal_id: TerminalId,
    instance: u64,
    synchronization_epoch: u64,
    size: (u16, u16),
    revision: u64,
    scroll_offset: Option<usize>,
    alternate_screen: bool,
    graphics: bool,
}

struct CapturedTab {
    workspace: usize,
    tab: usize,
    geometry: Vec<(crate::layout::PaneId, ratatui::layout::Rect)>,
    zoomed: bool,
    focused: crate::layout::PaneId,
}

pub(super) struct FeedbackCapture {
    epoch: u64,
    boot_id: String,
    projection: u64,
    base_revision: u64,
    terminal_size: (u16, u16),
    cell_size: crate::kitty_graphics::HostCellSize,
    runtimes: Vec<CapturedRuntime>,
    tabs: Vec<CapturedTab>,
    location_active: bool,
    focused_workspace: Option<String>,
    active_tabs: Vec<(String, Option<String>)>,
}

impl FeedbackCapture {
    /// Capture before collecting rows. Unselected revisions belong to the baseline.
    pub(super) fn new(app: &App, client: &ClientConnection) -> Option<Self> {
        let baseline = client.render_state.last_pane_surface()?;
        if baseline.panes.len() > MAX_CAPTURED_PANES {
            return None;
        }
        let mut runtimes = Vec::with_capacity(baseline.panes.len());
        let mut tabs: Vec<CapturedTab> = Vec::new();
        for pane in &baseline.panes {
            let (workspace, id) = app.parse_pane_id(&pane.pane_id)?;
            let terminal_id = app
                .state
                .workspaces
                .get(workspace)?
                .terminal_id(id)?
                .clone();
            let runtime =
                app.state
                    .runtime_for_pane_in_workspace(&app.terminal_runtimes, workspace, id)?;
            let (synchronized, synchronization_epoch) = runtime.synchronized_output_state();
            if synchronized {
                return None;
            }
            let tab_index = app.state.workspaces[workspace]
                .tabs
                .iter()
                .position(|tab| tab.panes.contains_key(&id))?;
            if !tabs
                .iter()
                .any(|tab| tab.workspace == workspace && tab.tab == tab_index)
            {
                let tab = &app.state.workspaces[workspace].tabs[tab_index];
                if tab.layout.pane_count() > MAX_CAPTURED_PANES {
                    return None;
                }
                let area = ratatui::layout::Rect::new(
                    0,
                    0,
                    client.terminal_size.0,
                    client.terminal_size.1,
                );
                tabs.push(CapturedTab {
                    workspace,
                    tab: tab_index,
                    geometry: tab
                        .layout
                        .panes(area)
                        .into_iter()
                        .map(|pane| (pane.id, pane.rect))
                        .collect(),
                    zoomed: tab.zoomed,
                    focused: tab.layout.focused(),
                });
            }
            runtimes.push(CapturedRuntime {
                pane_id: pane.pane_id.clone(),
                terminal_id,
                instance: runtime.runtime_instance(),
                synchronization_epoch,
                size: runtime.current_size(),
                revision: pane.content_revision,
                scroll_offset: runtime
                    .scroll_metrics()
                    .map(|scroll| scroll.offset_from_bottom),
                alternate_screen: runtime.alternate_screen_active(),
                graphics: runtime.graphics_may_have_placements(),
            });
        }
        let location = client.shell_location.as_ref();
        let active_tabs = tabs
            .iter()
            .map(|tab| {
                let workspace = app.state.workspaces[tab.workspace].id.clone();
                let active = location
                    .and_then(|location| location.active_tab_ids.get(&workspace))
                    .cloned();
                (workspace, active)
            })
            .collect();
        Some(Self {
            location_active: location.is_some(),
            focused_workspace: location.and_then(|location| location.focused_workspace_id.clone()),
            active_tabs,
            epoch: client.render_state.delivery_epoch()?,
            boot_id: baseline.boot_id.clone(),
            projection: baseline.projection_revision,
            base_revision: baseline.surface_revision,
            terminal_size: client.terminal_size,
            cell_size: client.cell_size,
            runtimes,
            tabs,
        })
    }

    pub(super) fn incorporate(&mut self, pane_id: &str, revision: u64, synchronization_epoch: u64) {
        if let Some(runtime) = self
            .runtimes
            .iter_mut()
            .find(|runtime| runtime.pane_id == pane_id)
        {
            runtime.revision = revision;
            runtime.synchronization_epoch = synchronization_epoch;
        }
    }

    pub(super) fn valid(&self, app: &App, client: &ClientConnection) -> bool {
        if app.state.popup_pane.is_some()
            || !client.is_active_shell_client()
            || client.terminal_size != self.terminal_size
            || client.cell_size != self.cell_size
            || client.render_state.delivery_epoch() != Some(self.epoch)
            || client.render_state.requires_recompute()
            || client.shell_projection_revision != self.projection
            || client
                .render_state
                .last_pane_surface()
                .is_none_or(|baseline| {
                    baseline.boot_id != self.boot_id
                        || baseline.projection_revision != self.projection
                        || baseline.surface_revision != self.base_revision
                })
        {
            return false;
        }
        let location = client.shell_location.as_ref();
        if location.is_some() != self.location_active
            || location.and_then(|location| location.focused_workspace_id.as_ref())
                != self.focused_workspace.as_ref()
            || self.active_tabs.iter().any(|(workspace, tab)| {
                location.and_then(|location| location.active_tab_ids.get(workspace)) != tab.as_ref()
            })
        {
            return false;
        }
        let area = ratatui::layout::Rect::new(0, 0, client.terminal_size.0, client.terminal_size.1);
        self.tabs.iter().all(|captured| {
            let Some(tab) = app
                .state
                .workspaces
                .get(captured.workspace)
                .and_then(|workspace| workspace.tabs.get(captured.tab))
            else {
                return false;
            };
            tab.zoomed == captured.zoomed
                && tab.layout.focused() == captured.focused
                && tab
                    .layout
                    .panes(area)
                    .into_iter()
                    .map(|pane| (pane.id, pane.rect))
                    .eq(captured.geometry.iter().copied())
        }) && self.runtimes.iter().all(|captured| {
            let Some((workspace, pane)) = app.parse_pane_id(&captured.pane_id) else {
                return false;
            };
            let Some(runtime) =
                app.state
                    .runtime_for_pane_in_workspace(&app.terminal_runtimes, workspace, pane)
            else {
                return false;
            };
            app.state.workspaces[workspace].terminal_id(pane) == Some(&captured.terminal_id)
                && runtime.runtime_instance() == captured.instance
                && runtime.current_size() == captured.size
                && runtime.synchronized_output_state() == (false, captured.synchronization_epoch)
                && runtime
                    .scroll_metrics()
                    .map(|scroll| scroll.offset_from_bottom)
                    == captured.scroll_offset
                && runtime.alternate_screen_active() == captured.alternate_screen
                && runtime.graphics_may_have_placements() == captured.graphics
        })
    }

    fn storage(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<Self>() + self.boot_id.capacity();
        bytes = bytes.checked_add(
            self.runtimes
                .capacity()
                .checked_mul(std::mem::size_of::<CapturedRuntime>())?,
        )?;
        for runtime in &self.runtimes {
            bytes = bytes
                .checked_add(runtime.pane_id.capacity())?
                .checked_add(runtime.terminal_id.as_str().len())?;
        }
        bytes = bytes.checked_add(
            self.tabs
                .capacity()
                .checked_mul(std::mem::size_of::<CapturedTab>())?,
        )?;
        for tab in &self.tabs {
            bytes = bytes.checked_add(tab.geometry.capacity().checked_mul(
                std::mem::size_of::<(crate::layout::PaneId, ratatui::layout::Rect)>(),
            )?)?;
        }
        bytes = bytes
            .checked_add(self.focused_workspace.as_ref().map_or(0, String::capacity))?
            .checked_add(
                self.active_tabs
                    .capacity()
                    .checked_mul(std::mem::size_of::<(String, Option<String>)>())?,
            )?;
        for (workspace, tab) in &self.active_tabs {
            bytes = bytes
                .checked_add(workspace.capacity())?
                .checked_add(tab.as_ref().map_or(0, String::capacity))?;
        }
        Some(bytes)
    }

    /// Dirty rows consumed by healthy peers cannot erase this recipient's follow-up.
    pub(super) fn newer_output(&self, app: &App) -> bool {
        self.runtimes.iter().any(|captured| {
            app.parse_pane_id(&captured.pane_id)
                .and_then(|(workspace, pane)| {
                    app.state
                        .runtime_for_pane_in_workspace(&app.terminal_runtimes, workspace, pane)
                })
                .is_some_and(|runtime| runtime.content_seq() != captured.revision)
        })
    }
}

pub(crate) struct PendingFeedback {
    pub(super) prepared: PreparedRender,
    pub(super) bytes: Vec<u8>,
    pub(super) trace: Vec<crate::latency_prof::SerializedFrame>,
    pub(super) receipts: TerminalReceipts,
    pub(super) capture: FeedbackCapture,
}

impl PendingFeedback {
    pub(super) fn new(
        capture: FeedbackCapture,
        prepared: PreparedRender,
        bytes: Vec<u8>,
        trace: Vec<crate::latency_prof::SerializedFrame>,
        receipts: TerminalReceipts,
    ) -> Option<Self> {
        let patch = prepared.retained_text_patch()?;
        if patch.boot_id != capture.boot_id
            || patch.projection_revision != capture.projection
            || patch.base_surface_revision != capture.base_revision
            || bytes.len() > protocol::MAX_FRAME_SIZE + 4
        {
            return None;
        }
        let stored = prepared
            .retained_text_storage()?
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(bytes.capacity())?
            .checked_add(capture.storage()?)?
            .checked_add(
                trace
                    .capacity()
                    .checked_mul(std::mem::size_of::<crate::latency_prof::SerializedFrame>())?,
            )?
            .checked_add(receipts.storage()?)?;
        if stored > 2 * protocol::MAX_FRAME_SIZE {
            return None;
        }
        Some(Self {
            prepared,
            bytes,
            trace,
            receipts,
            capture,
        })
    }
}
