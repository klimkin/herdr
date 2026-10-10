use super::*;

fn foreground_input_server() -> (HeadlessServer, tokio::sync::mpsc::Receiver<Bytes>) {
    let mut server = test_headless_server();
    let input = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
    connect_test_shell(&mut server, 1, 80, 24);
    (server, input)
}

#[tokio::test]
async fn repeated_foreground_input_preserves_keybindings_and_activity() {
    let (mut server, mut input) = foreground_input_server();
    server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 1,
        focused: true,
    });
    while input.try_recv().is_ok() {}
    let public_pane = server.app.session_snapshot().focused_pane_id.unwrap();
    let pane = server.app.state.workspaces[0].tabs[0].root_pane;
    let prefix_storage = server.app.state.prefix_keys.as_ptr();
    assert!(!server.app.state.prefix_keys.is_empty());
    let activity = server.clients[&1].last_activity;

    for text in ["first", "second"] {
        server.app.state.workspaces[0]
            .pane_state_mut(pane)
            .unwrap()
            .seen = false;
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: public_pane.clone(),
            events: vec![protocol::ClientPaneInputEvent::TextCommit(text.into())],
        });
        assert_eq!(
            input.try_recv().expect("ordered input"),
            Bytes::copy_from_slice(text.as_bytes())
        );
        // Replacement allocates before releasing the old nonempty vector.
        assert_eq!(server.app.state.prefix_keys.as_ptr(), prefix_storage);
        assert!(
            server.app.state.workspaces[0]
                .pane_state(pane)
                .unwrap()
                .seen
        );
    }
    assert!(server.clients[&1].last_activity > activity);
    assert_eq!(server.foreground_client_id, Some(1));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn foreground_input_keeps_same_client_resize_and_focus_updates() {
    let (mut server, mut input) = foreground_input_server();
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 1,
        surface_cols: 100,
        surface_rows: 30,
        cell_width_px: 8,
        cell_height_px: 16,
        pixel_mouse: true,
    }));
    assert_eq!(server.effective_size, (100, 30));
    assert!(server.app.pixel_mouse_available);
    assert_eq!(
        server.clients[&1].cell_size,
        crate::kitty_graphics::HostCellSize {
            width_px: 8,
            height_px: 16
        }
    );
    for focused in [true, false, true] {
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 1,
            focused
        }));
        assert_eq!(server.app.state.outer_terminal_focus, Some(focused));
        assert_eq!(
            input.try_recv().expect("focus report"),
            Bytes::from_static(if focused { b"\x1b[I" } else { b"\x1b[O" })
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn foreground_input_switches_client_and_resynchronizes_state() {
    let (mut server, mut input) = foreground_input_server();
    connect_test_shell(&mut server, 2, 100, 30);
    let public_pane = server.app.session_snapshot().focused_pane_id.unwrap();
    assert_eq!(server.foreground_client_id, Some(2));
    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: public_pane,
            events: vec![protocol::ClientPaneInputEvent::TextCommit("switch".into())],
        })
    );
    assert_eq!(
        input.try_recv().expect("switch input"),
        Bytes::from_static(b"switch")
    );
    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.effective_size, (80, 24));
    shutdown_test_runtimes(&mut server);
}
