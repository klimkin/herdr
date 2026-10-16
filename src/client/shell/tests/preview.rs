use super::*;
use crate::client::frame_output::FramePresentation;

fn text_shell(text: &str) -> ClientShellState {
    let mut shell = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let snapshot = super::super::tests::snapshot();
    shell.set_endpoint_snapshot_for_generation(&ClientEndpointId::Local, 1, Box::new(snapshot));
    let size = shell.surface_size(100, 30);
    let mut buffer = Buffer::empty(Rect::new(0, 0, size.cols, size.rows));
    buffer.set_string(1, 2, text, Style::default());
    let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    frame.cursor = Some(crate::protocol::CursorState {
        x: 1,
        y: 2,
        visible: true,
        shape: 2,
    });
    frame.graphics = b"do not replay graphics".to_vec();
    shell.set_pane_surface(PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: 1,
        surface_revision: 1,
        frame,
        panes: Vec::new(),
        splits: Vec::new(),
        popup: None,
        graphics: Default::default(),
    });
    shell
}

fn text(frame: &FrameData) -> String {
    frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

#[test]
fn retained_preview_displays_text_without_changing_authority_or_effects() {
    let mut shell = text_shell("retained terminal text");
    shell.remember_text_preview(100, 30);
    let snapshot = shell.snapshot.clone();
    let surface = shell.pane_surface.clone();
    shell.select_pending_preview(
        &ClientEndpointId::Local,
        Some(1),
        Some(ClientEndpointFocusTarget::Pane("pane_1".into())),
    );
    let frame = shell.compose(100, 30).unwrap();
    assert!(text(&frame).contains("retained terminal text"));
    assert_eq!(frame.presentation, FramePresentation::Preview);
    assert!(frame.cursor.is_none());
    assert!(frame.graphics.is_empty());
    assert!(frame.frame.graphics.is_empty());
    assert!(frame.hyperlinks.is_empty());
    assert!(shell.hits.panes.is_empty());
    assert!(shell.hits.pane_splits.is_empty());
    assert!(shell.hits.popup.is_none());
    assert!(shell.hits.tabs.is_empty());
    assert_eq!(shell.snapshot, snapshot);
    assert_eq!(shell.pane_surface, surface);
    assert_eq!(shell.active_endpoint_id, ClientEndpointId::Local);
}

#[test]
fn preview_misses_after_generation_boot_geometry_or_focus_changes() {
    for change in 0..5 {
        let mut shell = text_shell("outdated terminal text");
        shell.remember_text_preview(100, 30);
        let mut generation = 1;
        let mut size = (100, 30);
        let mut target = "pane_1";
        match change {
            0 => generation = 2,
            1 => shell.endpoints[0].snapshot.as_mut().unwrap().boot_id = "new-boot".into(),
            2 => size = (101, 30),
            3 => target = "missing-pane",
            _ => shell.endpoints[0].snapshot.as_mut().unwrap().panes.clear(),
        }
        shell.select_pending_preview(
            &ClientEndpointId::Local,
            Some(generation),
            Some(ClientEndpointFocusTarget::Pane(target.into())),
        );
        let frame = shell.compose(size.0, size.1).unwrap();
        assert!(
            !text(&frame).contains("outdated terminal text"),
            "change {change}"
        );
        assert!(text(&frame).contains("syncing"), "change {change}");
    }
}

#[test]
fn preview_does_not_replace_cached_coherent_text() {
    let mut shell = text_shell("coherent terminal text");
    shell.remember_text_preview(100, 30);
    shell.select_pending_preview(&ClientEndpointId::Local, Some(1), None);
    shell.compose(100, 30).unwrap();
    shell.remember_text_preview(100, 30);
    assert_eq!(shell.previews.entries.len(), 1);
    assert!(text(&shell.previews.entries[0].frame).contains("coherent terminal text"));
}

#[test]
fn frozen_preview_updates_host_text_and_defers_native_cleanup() {
    let mut state = crate::client::ClientState::test_new();
    state.shell = Some(text_shell("instant preview"));
    let shell = state.shell.as_mut().unwrap();
    shell.remember_text_preview(100, 30);
    shell.select_pending_preview(&ClientEndpointId::Local, Some(1), None);
    let frame = shell.compose(100, 30).unwrap();
    state.freeze_presentation();
    state.kitty_graphics_enabled = true;
    state.pending_native_cleanup = b"deferred graphics cleanup".to_vec();
    let mut output = Vec::new();
    assert!(state.try_present_frame_to(&mut output, frame));
    assert!(String::from_utf8_lossy(&output).contains("instant preview"));
    assert!(!String::from_utf8_lossy(&output).contains("deferred graphics cleanup"));
    assert!(state.presentation_frozen);
    assert_eq!(state.pending_native_cleanup, b"deferred graphics cleanup");
}

#[test]
fn coherent_focus_replaces_preview_only_for_latest_selection() {
    let mut shell = text_shell("coherent text");
    shell.select_pending_preview(
        &ClientEndpointId::Local,
        Some(1),
        Some(ClientEndpointFocusTarget::Pane("other-pane".into())),
    );
    shell.finish_coherent_preview();
    assert!(shell.previews.pending.is_some());
    shell.select_pending_preview(
        &ClientEndpointId::Local,
        Some(1),
        Some(ClientEndpointFocusTarget::Pane("pane_1".into())),
    );
    shell.finish_coherent_preview();
    assert!(shell.previews.pending.is_none());
    assert_eq!(
        shell.compose(100, 30).unwrap().presentation,
        FramePresentation::Coherent
    );
}

#[test]
fn preview_cache_limits_entries_bytes_and_removed_endpoints() {
    let mut shell = text_shell("bounded text");
    for index in 0..MAX_PREVIEWS + 3 {
        shell.snapshot.as_mut().unwrap().focused_pane_id = Some(format!("pane_{index}"));
        shell.remember_text_preview(100, 30);
    }
    assert_eq!(shell.previews.entries.len(), MAX_PREVIEWS);
    assert!(
        shell
            .previews
            .entries
            .iter()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            <= MAX_PREVIEW_BYTES
    );
    let mut oversized = shell.previews.entries.pop_back().unwrap();
    oversized.bytes = MAX_PREVIEW_BYTES + 1;
    shell.previews.insert(oversized);
    assert!(
        shell
            .previews
            .entries
            .iter()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            <= MAX_PREVIEW_BYTES
    );
    shell.previews.retain_endpoints(&[]);
    assert!(shell.previews.entries.is_empty());
}

#[test]
fn pending_preview_keeps_help_settings_and_mobile_navigation_visible() {
    for action in [
        crate::input::KeybindAction::Help,
        crate::input::KeybindAction::Settings,
    ] {
        let mut shell = text_shell("source");
        shell.select_pending_preview(&ClientEndpointId::Local, Some(1), None);
        let mut outcome = ClientShellInput::default();
        shell.record_binding(crate::input::KeybindMatch::Action(action), &mut outcome);
        let frame = shell.compose(100, 30).unwrap();
        assert_eq!(frame.presentation, FramePresentation::Preview);
        let popup = match action {
            crate::input::KeybindAction::Help => shell.hits.help_popup,
            crate::input::KeybindAction::Settings => shell.hits.settings_popup,
            _ => unreachable!(),
        };
        assert!(
            !popup.is_empty(),
            "{action:?} must render while synchronizing"
        );
    }
    let mut shell = text_shell("source");
    shell.config.mobile_width_threshold = 80;
    shell.select_pending_preview(&ClientEndpointId::Local, Some(1), None);
    shell.compose(40, 24).unwrap();
    assert!(!shell.hits.mobile_switch.is_empty());
    shell.mode = ClientShellMode::Navigate;
    shell.compose(40, 24).unwrap();
    assert!(shell
        .hits
        .mobile_targets
        .iter()
        .any(|(_, target)| matches!(target, ClientMobileTarget::Workspace { .. })));
    assert!(!shell.hits.mobile_close.is_empty());
}
