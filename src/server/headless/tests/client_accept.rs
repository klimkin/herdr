use super::*;
use interprocess::local_socket::traits::Stream as _;

#[cfg(unix)]
#[test]
fn empty_client_listener_reports_would_block_without_admission() {
    let mut server = test_headless_server();
    let stats = accept_pending_client_connections(
        &server.client_listener,
        &mut server.next_client_id,
        &server.should_quit,
        &server.server_event_tx,
    )
    .expect("inspect empty listener");
    assert_eq!(stats.attempted, 1);
    assert_eq!(stats.would_block, 1);
    assert_eq!(stats.accepted, 0);
    assert_eq!(server.next_client_id, 1);
    assert!(server.server_event_rx.is_empty());
}

fn hello_client(server: &HeadlessServer) -> crate::ipc::LocalStream {
    let mut stream = crate::ipc::connect_local_stream(&server.client_socket_path)
        .expect("connect client to owned listener");
    stream
        .set_recv_timeout(Some(Duration::from_secs(3)))
        .expect("bound test receive");
    protocol::write_message(
        &mut stream,
        &protocol::ClientMessage::TerminalHello {
            version: protocol::PROTOCOL_VERSION,
            cols: 120,
            rows: 40,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        },
    )
    .expect("send compatible hello");
    stream
}

fn read_welcome(stream: &mut crate::ipc::LocalStream) {
    let message: ServerMessage =
        protocol::read_message(stream, MAX_FRAME_SIZE).expect("receive welcome");
    assert!(matches!(
        message,
        ServerMessage::Welcome { error: None, .. }
    ));
}

#[test]
fn empty_accepts_scale_with_unrelated_server_turns() {
    let mut server = test_headless_server();
    let mut would_block = 0;
    for _ in 0..64 {
        let stats = accept_pending_client_connections(
            &server.client_listener,
            &mut server.next_client_id,
            &server.should_quit,
            &server.server_event_tx,
        )
        .expect("inspect unrelated event turn");
        would_block += stats.would_block;
    }
    assert_eq!(would_block, 64);
    assert!(server.server_event_rx.is_empty());
}

#[test]
fn client_attach_survives_empty_queue_and_rejects_invalid_peer() {
    let mut server = test_headless_server();
    server
        .accept_client_connections()
        .expect("drain empty queue");
    let mut malformed = crate::ipc::connect_local_stream(&server.client_socket_path).unwrap();
    malformed
        .set_recv_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    protocol::write_message(&mut malformed, &protocol::ClientMessage::Detach).unwrap();
    let mut valid = hello_client(&server);
    server
        .accept_client_connections()
        .expect("drain pending clients");
    read_welcome(&mut valid);
    let message: ServerMessage = protocol::read_message(&mut malformed, MAX_FRAME_SIZE).unwrap();
    assert!(matches!(
        message,
        ServerMessage::Welcome { error: Some(_), .. }
    ));
    let event = server
        .server_event_rx
        .blocking_recv()
        .expect("valid admission");
    assert!(matches!(
        event,
        ServerEvent::ClientConnected { client_id: 2, .. }
    ));
    assert_eq!(server.next_client_id, 3);
}

#[test]
fn client_connection_burst_receives_distinct_admissions() {
    let mut server = test_headless_server();
    let mut peers: Vec<_> = (0..17).map(|_| hello_client(&server)).collect();
    server
        .accept_client_connections()
        .expect("drain connection burst");
    for peer in &mut peers {
        read_welcome(peer);
    }
    let mut ids = std::collections::BTreeSet::new();
    for _ in &peers {
        let ServerEvent::ClientConnected { client_id, .. } = server
            .server_event_rx
            .blocking_recv()
            .expect("burst admission")
        else {
            panic!("expected distinct client admission");
        };
        assert!(ids.insert(client_id));
    }
    assert_eq!(ids, (1..=17).collect());
}

#[test]
fn shutdown_closes_listener_with_pending_connection() {
    let mut server = test_headless_server();
    let mut pending = hello_client(&server);
    server.should_quit.store(true, Ordering::Release);
    server
        .accept_client_connections()
        .expect("stop prevents admission");
    assert_eq!(server.next_client_id, 1);
    assert!(server.server_event_rx.is_empty());
    drop(server);
    assert!(protocol::read_message::<_, ServerMessage>(&mut pending, MAX_FRAME_SIZE).is_err());
}

#[test]
fn failed_client_registration_sends_shutdown_to_accepted_peer() {
    let mut server = test_headless_server();
    server.server_event_rx.close();
    let mut peer = hello_client(&server);
    server
        .accept_client_connections()
        .expect("accept with closed registration channel");
    read_welcome(&mut peer);
    let message: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
    assert!(matches!(message, ServerMessage::ServerShutdown { .. }));
}
