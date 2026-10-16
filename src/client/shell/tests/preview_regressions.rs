use super::*;
use crate::client::frame_output::{ComposedFrame, FramePresentation};

fn displayed(h: &mut Harness) -> ComposedFrame {
    let frame = h.state.shell.as_mut().unwrap().compose(100, 30).unwrap();
    assert!(
        h.state.blit_encoder.is_current(&frame.frame),
        "preview was composed but not presented"
    );
    frame
}

fn text(frame: &FrameData) -> String {
    frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

fn assert_workspace_highlight(
    h: &Harness,
    frame: &FrameData,
    endpoint: &ClientEndpointId,
    id: &str,
) {
    let hit = h
        .shell()
        .hits
        .workspaces
        .iter()
        .find(|hit| &hit.endpoint_id == endpoint && hit.workspace_id == id)
        .unwrap();
    let buffer = frame.to_ratatui_buffer().unwrap();
    assert_eq!(
        buffer[(hit.rect.x + 2, hit.rect.y)].bg,
        h.shell().config.palette.selection_bg
    );
}

#[test]
fn selection_and_rapid_retarget_are_presented_before_server_ack() {
    let mut h = Harness::new();
    let remote = h.remote_id();
    let original = h.shell().snapshot.clone();
    h.select_target(
        remote.clone(),
        ClientEndpointFocusTarget::Workspace("remote_1".into()),
    );
    let first = displayed(&mut h);
    assert_eq!(first.presentation, FramePresentation::Preview);
    assert!(text(&first).contains("syncing"));
    assert_workspace_highlight(&h, &first, &remote, "remote_1");
    h.select_target(
        remote.clone(),
        ClientEndpointFocusTarget::Workspace("remote_2".into()),
    );
    let latest = displayed(&mut h);
    assert_workspace_highlight(&h, &latest, &remote, "remote_2");
    assert_eq!(h.shell().snapshot, original);
    assert_eq!(h.endpoints.active_id(), &ClientEndpointId::Local);
    assert!(!h.endpoints.active_surface_available());
    h.settle();
    assert_eq!(h.remote.focused, "remote_2");
    assert!(!h.shell().has_pending_preview());
}

#[test]
fn revisiting_machine_presents_retained_text_before_ack() {
    let mut h = harness_on_remote_last_workspace();
    let remote = h.remote_id();
    let shell = h.state.shell.as_mut().unwrap();
    let surface = shell.pane_surface.as_mut().unwrap();
    let mut buffer = surface.frame.to_ratatui_buffer().unwrap();
    buffer.set_string(1, 2, "remembered remote output", Style::default());
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    let coherent = shell.compose(100, 30).unwrap();
    h.state.present_frame(coherent);
    h.select_machine(ClientEndpointId::Local);
    h.settle();
    h.select_machine(remote);
    let frame = displayed(&mut h);
    assert!(text(&frame).contains("remembered remote output"));
    assert!(h.state.presentation_frozen);
    assert!(h.handoff_in_progress());
    assert_eq!(h.endpoints.active_id(), &ClientEndpointId::Local);
    assert!(h.shell().hits.panes.is_empty());
}

#[test]
fn pending_agent_selection_highlights_latest_row_and_blocks_typing() {
    let mut h = Harness::new();
    let remote = h.remote_id();
    h.select_pane(remote.clone(), "remote_1:pane");
    h.select_pane(remote.clone(), "remote_2:pane");
    let frame = displayed(&mut h);
    let hit = h
        .shell()
        .hits
        .endpoint_agents
        .iter()
        .find(|(_, endpoint, pane)| endpoint == &remote && pane == "remote_2:pane")
        .unwrap();
    let buffer = frame.to_ratatui_buffer().unwrap();
    assert_eq!(
        buffer[(hit.0.x, hit.0.y)].bg,
        h.shell().config.palette.active_row_bg
    );
    let outcome = h
        .state
        .shell
        .as_mut()
        .unwrap()
        .handle_input_bytes(b"must not reach a pane");
    finish_client_shell_input(
        &mut h.state,
        outcome,
        None,
        &mut h.endpoints,
        &mut h.pending,
        &mut h.commands,
        &mut h.prefix,
        &mut h.scheduled,
    )
    .unwrap();
    for sent in [&h.local.inbox, &h.remote.inbox] {
        assert!(!sent.lock().unwrap().iter().any(|message| matches!(
            message,
            ClientMessage::ClientShellPaneInput { .. }
                | ClientMessage::ClientShellPopupInput { .. }
        )));
    }
    h.settle();
    assert_eq!(h.remote.focused, "remote_2");
}

#[test]
fn restored_source_does_not_replace_successor_preview() {
    let mut h = harness_on_remote_last_workspace();
    h.select_machine(ClientEndpointId::Local);
    h.run_until("Local projection", |h| {
        h.shell().endpoint_is_active(&ClientEndpointId::Local)
    });
    let remote = h.remote_id();
    h.select_pane(remote.clone(), "remote_1:pane");
    h.run_until("source restoration", |h| {
        h.shell().endpoint_is_active(&remote)
    });
    let frame = displayed(&mut h);
    assert_eq!(frame.presentation, FramePresentation::Preview);
    assert_workspace_highlight(&h, &frame, &remote, "remote_1");
    assert!(h.handoff_in_progress());
    h.settle();
    assert_eq!(h.remote.focused, "remote_1");
}
