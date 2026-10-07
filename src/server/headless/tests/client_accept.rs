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

#[test]
fn handoff_rejects_every_queued_peer_before_export() {
    let server = test_headless_server();
    let count = crate::server::client_accept::CLIENT_ACCEPT_BATCH_LIMIT + 9;
    let mut peers: Vec<_> = (0..count)
        .map(|_| {
            let stream = crate::ipc::connect_local_stream(&server.client_socket_path).unwrap();
            stream
                .set_recv_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            stream
        })
        .collect();
    let stats = reject_pending_client_connections(&server.client_listener).unwrap();
    for peer in &mut peers {
        let mut byte = [0];
        assert_eq!(std::io::Read::read(peer, &mut byte).unwrap(), 0);
    }
    assert_eq!(stats.accepted, count as u64);
    assert_eq!(stats.would_block, 1);
    assert_eq!(server.next_client_id, 1);
    assert!(server.server_event_rx.is_empty());
}

#[test]
fn handoff_runtime_turn_bounds_rejection_without_losing_queued_peers() {
    let mut server = test_headless_server();
    server.handoff_in_progress = true;
    let count = crate::server::client_accept::CLIENT_ACCEPT_BATCH_LIMIT + 1;
    let mut peers: Vec<_> = (0..count)
        .map(|_| crate::ipc::connect_local_stream(&server.client_socket_path).unwrap())
        .collect();
    let first = server.accept_client_connections().unwrap();
    assert_eq!(first.accepted, 32);
    assert_eq!(first.would_block, 0);
    let remaining = server.accept_client_connections().unwrap();
    assert_eq!(remaining.accepted, 1);
    assert_eq!(remaining.would_block, 1);
    for peer in &mut peers {
        let mut byte = [0];
        assert_eq!(std::io::Read::read(peer, &mut byte).unwrap(), 0);
    }
    assert_eq!(server.next_client_id, 1);
    assert!(server.server_event_rx.is_empty());
}

#[tokio::test]
async fn client_accept_readiness_rearms_after_empty_queue() {
    let mut server = test_headless_server();
    server.client_listener_ready =
        Some(crate::platform::LocalListenerReady::new(&server.client_listener).unwrap());
    server
        .accept_client_connections()
        .expect("empty readiness check");
    for expected in 1..=2 {
        let mut peer = hello_client(&server);
        server
            .client_listener_ready
            .as_ref()
            .unwrap()
            .ready()
            .await
            .unwrap();
        server
            .accept_client_connections()
            .expect("accept notified peer");
        read_welcome(&mut peer);
        loop {
            if let ServerEvent::ClientConnected { client_id, .. } =
                server.server_event_rx.recv().await.unwrap()
            {
                assert_eq!(client_id, expected);
                break;
            }
        }
        server
            .accept_client_connections()
            .expect("drain queue and clear readiness");
        assert!(server
            .client_listener_ready
            .as_ref()
            .unwrap()
            .try_ready()
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn client_accept_readiness_retains_a_burst_without_new_connections() {
    let mut server = test_headless_server();
    server.client_listener_ready =
        Some(crate::platform::LocalListenerReady::new(&server.client_listener).unwrap());
    let count = crate::server::client_accept::CLIENT_ACCEPT_BATCH_LIMIT + 1;
    let mut peers: Vec<_> = (0..count).map(|_| hello_client(&server)).collect();
    server
        .client_listener_ready
        .as_ref()
        .unwrap()
        .ready()
        .await
        .unwrap();
    server
        .accept_client_connections()
        .expect("first fair batch");
    assert_eq!(
        server.next_client_id,
        crate::server::client_accept::CLIENT_ACCEPT_BATCH_LIMIT as u64 + 1
    );
    // No peer connects after this point: retained readiness must keep progress.
    server
        .accept_client_connections()
        .expect("finish burst from retained readiness");
    for peer in &mut peers {
        read_welcome(peer);
    }
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..count {
        let ServerEvent::ClientConnected { client_id, .. } =
            server.server_event_rx.recv().await.unwrap()
        else {
            panic!("expected admission from fair burst");
        };
        assert!(ids.insert(client_id));
    }
    assert_eq!(ids.len(), count);
}

#[tokio::test]
async fn client_accept_loop_wakes_from_idle_and_survives_output_signals() {
    let mut server = test_headless_server();
    let path = server.client_socket_path.clone();
    // Keep the API sender alive: a closed channel is not an idle runtime.
    let (api_tx, api_rx) = mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    let render_notify = server.app.render_notify.clone();
    let render_dirty = server.app.render_dirty.clone();
    let (reached_peer, peer_reached) = tokio::sync::oneshot::channel();
    let peer = tokio::task::spawn_blocking(move || {
        let mut stream = crate::ipc::connect_local_stream(&path).unwrap();
        stream
            .set_recv_timeout(Some(Duration::from_secs(3)))
            .unwrap();
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
        .unwrap();
        read_welcome(&mut stream);
        reached_peer.send(()).unwrap();
        stream
    });
    let producer = tokio::spawn(async move {
        for _ in 0..256 {
            render_dirty.request_generic();
            render_notify.notify_one();
            tokio::task::yield_now().await;
        }
    });
    let stop =
        tokio::spawn(async move {
            peer_reached.await.unwrap();
            let (respond_to, _) = std::sync::mpsc::channel();
            api_tx.send(api::ApiRequestMessage {
            #[cfg(feature = "latency-prof")]
            diagnostic_trace: crate::latency_prof::event::EventTrace::default(),
            request: api::schema::Request {
                id: "listener-stop".into(),
                method: api::schema::Method::ServerStop(api::schema::EmptyParams::default()),
            }, respond_to, response_write_complete: None,
        }).unwrap();
        });
    tokio::time::timeout(Duration::from_secs(5), server.run())
        .await
        .unwrap()
        .unwrap();
    stop.await.unwrap();
    producer.await.unwrap();
    drop(peer.await.unwrap());
}

#[tokio::test]
async fn client_accept_idle_readiness_never_attempts_empty_accepts() {
    let mut server = test_headless_server();
    server.client_listener_ready =
        Some(crate::platform::LocalListenerReady::new(&server.client_listener).unwrap());
    for _ in 0..64 {
        let stats = server
            .accept_client_connections()
            .expect("check unrelated event readiness");
        assert_eq!(stats.attempted, 0);
    }
    assert_eq!(server.next_client_id, 1);
    assert!(server.server_event_rx.is_empty());
    assert!(server
        .client_listener_ready
        .as_ref()
        .unwrap()
        .try_ready()
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn client_accept_cancelled_idle_wait_preserves_future_connection() {
    let mut server = test_headless_server();
    server.client_listener_ready =
        Some(crate::platform::LocalListenerReady::new(&server.client_listener).unwrap());
    let ready = server.client_listener_ready.as_ref().unwrap();
    let mut waiting = Box::pin(ready.ready());
    assert!(matches!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            waiting.as_mut(),
            cx
        )))
        .await,
        std::task::Poll::Pending
    ));
    drop(waiting);
    let mut peer = hello_client(&server);
    server
        .client_listener_ready
        .as_ref()
        .unwrap()
        .ready()
        .await
        .unwrap();
    server
        .accept_client_connections()
        .expect("admit after cancelled idle wait");
    read_welcome(&mut peer);
    assert!(matches!(
        server.server_event_rx.recv().await,
        Some(ServerEvent::ClientConnected { client_id: 1, .. })
    ));
}

#[tokio::test]
async fn client_accept_error_retry_yields_without_stopping_api_progress() {
    let mut server = test_headless_server();
    server.client_accept_retry_at = Some(Instant::now() + Duration::from_secs(60));
    let mut peer = hello_client(&server);
    for _ in 0..64 {
        let stats = server
            .accept_client_connections()
            .expect("failed listener yields runtime turns");
        assert_eq!(stats.attempted, 0);
    }
    let (api_tx, api_rx) = mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    let (respond_to, response) = std::sync::mpsc::channel();
    api_tx
        .send(api::ApiRequestMessage {
            #[cfg(feature = "latency-prof")]
            diagnostic_trace: crate::latency_prof::event::EventTrace::default(),
            request: api::schema::Request {
                id: "unaffected-api".into(),
                method: api::schema::Method::WorkspaceList(api::schema::EmptyParams::default()),
            },
            respond_to,
            response_write_complete: None,
        })
        .unwrap();
    server.drain_api_requests_with_shutdown_check();
    assert!(
        serde_json::from_str::<serde_json::Value>(&response.recv().unwrap())
            .unwrap()
            .get("result")
            .is_some()
    );
    // Deadline expiry retries the retained connection, preserving listener progress.
    server.client_accept_retry_at = Some(Instant::now());
    let stats = server
        .accept_client_connections()
        .expect("retry queued peer");
    assert_eq!(stats.accepted, 1);
    read_welcome(&mut peer);
    assert!(matches!(
        server.server_event_rx.recv().await,
        Some(ServerEvent::ClientConnected { client_id: 1, .. })
    ));
    assert!(server.client_accept_retry_at.is_none());
}

#[test]
fn client_accept_resource_exhaustion_retries_without_server_exit() {
    const CHILD: &str = "HERDR_TEST_ACCEPT_RESOURCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "server::headless::tests::client_accept::client_accept_resource_exhaustion_retries_without_server_exit", "--nocapture"])
            .env(CHILD, "1")
            .status().unwrap();
        assert!(status.success(), "isolated descriptor-limit test passes");
        return;
    }
    let mut server = test_headless_server();
    let mut peer = hello_client(&server);
    let mut original = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, original.as_mut_ptr()) },
        0
    );
    let original = unsafe { original.assume_init() };
    // Only this child process receives the limit. Keep enough descriptors for
    // the existing client, then exhaust the remainder before calling accept.
    let limited = libc::rlimit {
        rlim_cur: 128.min(original.rlim_cur),
        rlim_max: original.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
    let mut files = Vec::new();
    loop {
        match fs::File::open("/dev/null") {
            Ok(file) => files.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    let stats = server.accept_client_connections().unwrap();
    assert_eq!(stats.failed, 1, "real accept reports descriptor exhaustion");
    assert!(server.client_accept_retry_at.is_some());
    assert!(!server.should_quit.load(Ordering::Acquire));
    assert_eq!(server.accept_client_connections().unwrap().attempted, 0);
    drop(files);
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
        0
    );
    server.client_accept_retry_at = Some(Instant::now());
    assert_eq!(server.accept_client_connections().unwrap().accepted, 1);
    read_welcome(&mut peer);
    assert!(matches!(
        server.server_event_rx.blocking_recv(),
        Some(ServerEvent::ClientConnected { client_id: 1, .. })
    ));
}
