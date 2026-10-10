use super::*;

#[tokio::test]
async fn render_drain_burst_retries_latest_deferred_output_once() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());

    writer.test_fill_render(vec![0]);
    write_shared_test_pane(&mut server, pane_id, b"\rLATEST");
    server.render_and_stream();
    assert_eq!(server.clients[&27].deferred_render(), DeferredRender::Full);
    writer.test_drain();

    // Several dequeues can precede one server batch. Only the first signal
    // consumes this deferred request; the replacement arrives without new PTY output.
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    server.render_and_stream();
    let messages = writer.test_drain();
    assert!(messages
        .into_iter()
        .map(read_server_message)
        .any(|message| {
            let ServerMessage::PaneSurface(surface) = message else {
                return false;
            };
            surface
                .frame
                .cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .contains("LATEST")
        }));
    assert_eq!(server.clients[&27].deferred_render(), DeferredRender::None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn render_drain_before_deferred_registration_does_not_lose_later_progress() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 28);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&28).unwrap().writer = Some(writer.clone());

    // The first feedback is handled before any new deferred work exists.
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 28 }));
    writer.test_fill_render(vec![0]);
    write_shared_test_pane(&mut server, pane_id, b"\rNEXT");
    server.render_and_stream();
    assert_eq!(server.clients[&28].deferred_render(), DeferredRender::Full);
    writer.test_drain();
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 28 }));
    server.render_and_stream();
    assert!(
        !writer.test_drain().is_empty(),
        "later slot progress delivers deferred output"
    );
    assert_eq!(server.clients[&28].deferred_render(), DeferredRender::None);

    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 28 });
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 28 }));
    shutdown_test_runtimes(&mut server);
}
