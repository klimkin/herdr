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

#[tokio::test]
async fn admitted_feedback_follows_queued_patch_without_full_recovery() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    let mut delivered = server.clients[&27]
        .render_state
        .last_pane_surface()
        .unwrap()
        .clone();
    write_shared_test_pane(&mut server, pane, b"\rOLDER");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane])));
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    let messages = writer.test_drain();
    assert_eq!(
        messages.len(),
        2,
        "admitted echo queues behind the older patch"
    );
    for bytes in messages {
        let ServerMessage::PaneSurfacePatch(patch) = read_server_message(bytes) else {
            panic!("retained text delivery must remain a patch");
        };
        assert_eq!(patch.base_surface_revision, delivered.surface_revision);
        crate::server::render_stream::apply_pane_surface_patch(&mut delivered, &patch);
    }
    assert!(delivered
        .frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>()
        .contains("ECHO"));
    assert_eq!(server.clients[&27].deferred_render(), DeferredRender::None);
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn admitted_feedback_retries_prepared_patch_on_drain_without_rebuilding() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    let baseline = server.clients[&27]
        .render_state
        .last_pane_surface()
        .unwrap()
        .clone();
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    assert_eq!(
        server.clients[&27].render_state.last_pane_surface(),
        Some(&baseline),
        "deferred delivery cannot commit its baseline"
    );
    let _ = writer.test_drain();
    assert!(
        !server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }),
        "delivery retry does not request a full render"
    );
    let messages = writer.test_drain();
    assert_eq!(
        messages.len(),
        1,
        "drain delivers the saved patch without output or rendering"
    );
    let ServerMessage::PaneSurfacePatch(patch) =
        read_server_message(messages.into_iter().next().unwrap())
    else {
        panic!("saved patch");
    };
    assert_eq!(patch.base_surface_revision, baseline.surface_revision);
    let mut delivered = baseline;
    crate::server::render_stream::apply_pane_surface_patch(&mut delivered, &patch);
    assert!(delivered
        .frame
        .cells
        .iter()
        .map(|c| c.symbol.as_str())
        .collect::<String>()
        .contains("ECHO"));
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(
        writer.test_drain().is_empty(),
        "duplicate drain cannot deliver twice"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_preserves_newer_output_and_healthy_peer_baselines() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let (peer_control, peer_render) = connect_matching_test_shell(&mut server, 28);
    let _ = control.recv().unwrap();
    let _ = peer_control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let _ = peer_render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    assert!(server.clients[&27].pending_feedback.is_some());
    let _ = peer_render.recv().unwrap();
    write_shared_test_pane(&mut server, pane, b"\rNEWER");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane])));
    let _ = peer_render.recv().unwrap();
    let peer_before = server.clients[&28]
        .render_state
        .last_pane_surface()
        .unwrap()
        .clone();
    let _ = writer.test_drain();
    assert!(
        !server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }),
        "recipient recovery does not request global rendering"
    );
    assert!(server.feedback_recovery.contains(&27));
    assert_eq!(
        server.clients[&28].render_state.last_pane_surface(),
        Some(&peer_before)
    );
    assert_eq!(writer.test_drain().len(), 1);
    server.render_feedback_recovery_traced(crate::latency_prof::TraceContext::default());
    assert!(
        peer_render.try_recv().is_err(),
        "healthy peer does not receive recovery frame"
    );
    assert_eq!(
        server.clients[&28].render_state.last_pane_surface(),
        Some(&peer_before)
    );
    let messages = writer.test_drain();
    assert!(
        !messages.is_empty(),
        "follow-up restores newer output after producer stops"
    );
    assert!(server.clients[&27]
        .render_state
        .last_pane_surface()
        .unwrap()
        .frame
        .cells
        .iter()
        .map(|c| c.symbol.as_str())
        .collect::<String>()
        .contains("NEWER"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_rejects_invalidated_baselines_and_runtime_transactions() {
    for invalidation in [
        "repaint",
        "projection",
        "geometry",
        "synchronization",
        "runtime",
        "source",
        "layout",
        "popup",
    ] {
        let mut server = test_headless_server();
        let pane = install_shared_view_test_runtime(&mut server);
        let (control, render) = connect_matching_test_shell(&mut server, 27);
        let _ = control.recv().unwrap();
        server.render_and_stream();
        let _ = render.recv().unwrap();
        let writer = ClientWriter::test_paused();
        server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
        writer
            .render
            .send_ordered(
                HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
            )
            .unwrap();
        write_shared_test_pane(&mut server, pane, b"\rECHO");
        let instance = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
            .unwrap()
            .runtime_instance();
        server.render_target_pane_surface_traced(
            pane,
            instance,
            crate::latency_prof::TraceContext::default(),
        );
        assert!(server.clients[&27].pending_feedback.is_some());
        match invalidation {
            "repaint" => server.clients.get_mut(&27).unwrap().request_repaint(),
            "projection" => {
                server
                    .clients
                    .get_mut(&27)
                    .unwrap()
                    .shell_projection_revision += 1
            }
            "geometry" => server.clients.get_mut(&27).unwrap().terminal_size.0 += 1,
            "synchronization" => {
                write_shared_test_pane(&mut server, pane, b"\x1b[?2026h");
                write_shared_test_pane(&mut server, pane, b"\x1b[?2026l");
            }
            "popup" => {
                server.app.install_test_popup_runtime(
                    crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"POPUP"),
                );
            }
            "source" => {
                server
                    .clients
                    .get_mut(&27)
                    .unwrap()
                    .shell_location
                    .as_mut()
                    .unwrap()
                    .focused_workspace_id = Some("removed".into());
            }
            "layout" => {
                server.app.state.workspaces[0].tabs[0].zoomed = true;
            }
            "runtime" => {
                server.app.state.workspaces[0].insert_test_runtime(
                    pane,
                    crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"NEW"),
                );
            }
            _ => unreachable!(),
        }
        let _ = writer.test_drain();
        assert!(
            !server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }),
            "{invalidation} stays recipient-local"
        );
        assert!(
            server.feedback_recovery.contains(&27),
            "{invalidation} requests safe recovery"
        );
        assert!(
            writer.test_drain().is_empty(),
            "{invalidation} cannot send a stale saved patch"
        );
        assert!(server.clients[&27].pending_feedback.is_none());
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn pending_feedback_stays_bounded_when_drain_does_not_free_ordered_lane() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    for _ in 0..20 {
        assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
        assert!(server.clients[&27].pending_feedback.is_some());
    }
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert_eq!(writer.test_drain().len(), 1);
    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 27 });
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_registration_keeps_constructed_revision_and_transaction() {
    for synchronized in [false, true] {
        let mut server = test_headless_server();
        let pane = install_shared_view_test_runtime(&mut server);
        let (control, render) = connect_matching_test_shell(&mut server, 27);
        let _ = control.recv().unwrap();
        server.render_and_stream();
        let _ = render.recv().unwrap();
        let writer = ClientWriter::test_paused();
        server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
        writer
            .render
            .send_ordered(
                HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
            )
            .unwrap();
        write_shared_test_pane(&mut server, pane, b"\rECHO");
        let instance = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
            .unwrap()
            .runtime_instance();
        server.render_target_pane_surface_traced(
            pane,
            instance,
            crate::latency_prof::TraceContext::default(),
        );
        // Move owned construction across the race window before registration.
        let pending = *server
            .clients
            .get_mut(&27)
            .unwrap()
            .pending_feedback
            .take()
            .unwrap();
        write_shared_test_pane(&mut server, pane, b"\rNEWER");
        if synchronized {
            write_shared_test_pane(&mut server, pane, b"\x1b[?2026h");
            write_shared_test_pane(&mut server, pane, b"\x1b[?2026l");
        }
        let saved = crate::server::feedback_delivery::PendingFeedback::new(
            pending.capture,
            pending.prepared,
            pending.bytes,
            pending.trace,
            pending.receipts,
        )
        .unwrap();
        server.clients.get_mut(&27).unwrap().pending_feedback = Some(Box::new(saved));
        let _ = writer.test_drain();
        assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
        let sent = writer.test_drain();
        assert_eq!(sent.len(), usize::from(!synchronized));
        assert!(
            server.feedback_recovery.contains(&27),
            "construction revision owns later-output recovery"
        );
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn pending_feedback_retry_keeps_original_budget_after_expiry() {
    use crate::app::early_presentation::EarlyPresentation;
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    server.app.early_presentation =
        EarlyPresentation::new(crate::latency_experiments::PresentationPolicy::ActionFullTarget);
    let now = Instant::now() + Duration::from_secs(1);
    server
        .app
        .early_presentation
        .accepted_action("w1".into(), "first".into(), "1".into(), now);
    let first = server
        .app
        .early_presentation
        .oldest_ready(now, |_| true)
        .unwrap();
    assert!(server.app.early_presentation.admit(first.id, now, false));
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    server.app.early_presentation.accepted_action(
        "w2".into(),
        "second".into(),
        "2".into(),
        now + Duration::from_millis(1),
    );
    let second = server
        .app
        .early_presentation
        .oldest_ready(now + Duration::from_millis(1), |_| true)
        .unwrap();
    assert!(!server
        .app
        .early_presentation
        .admit(second.id, now + Duration::from_millis(1), false));
    assert!(
        server.app.early_presentation.contains(second.id),
        "before retry"
    );
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(
        server.app.early_presentation.contains(second.id),
        "after retry"
    );
    assert!(
        !server
            .app
            .early_presentation
            .admit(second.id, now + Duration::from_millis(15), false),
        "delivery cannot refund original charge"
    );
    assert!(
        server
            .app
            .early_presentation
            .admit(second.id, now + Duration::from_millis(16), false),
        "delivery cannot charge another construction"
    );
    assert_eq!(writer.test_drain().len(), 1);
    // Saving and delivering do not consult opportunity lifetime.
    server
        .app
        .early_presentation
        .prune_terminals(now + Duration::from_secs(1), |_| true);
    assert!(!server.app.early_presentation.contains(first.id));
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rAFTER");
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    assert!(server.clients[&27].pending_feedback.is_some());
    server
        .app
        .early_presentation
        .prune_terminals(now + Duration::from_secs(2), |_| true);
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert_eq!(
        writer.test_drain().len(),
        1,
        "expiry cannot discard already admitted delivery"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_disconnect_releases_saved_delivery() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    assert!(server.clients[&27].pending_feedback.is_some());
    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 27 });
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(writer.test_drain().is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_viewport_scroll_cancels_old_patch() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let history = (0..50).map(|n| format!("line {n}\r\n")).collect::<String>();
    server.app.state.workspaces[0].insert_test_runtime(
        pane,
        crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
            80,
            23,
            1024 * 1024,
            history.as_bytes(),
        ),
    );
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"ECHO");
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap();
    let instance = runtime.runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    assert!(server.clients[&27].pending_feedback.is_some());
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .scroll_up(3);
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
            .unwrap()
            .scroll_metrics()
            .unwrap()
            .offset_from_bottom,
        3
    );
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(writer.test_drain().is_empty());
    assert!(server.feedback_recovery.contains(&27));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_rejects_oversized_owned_delivery() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    let mut pending = *server
        .clients
        .get_mut(&27)
        .unwrap()
        .pending_feedback
        .take()
        .unwrap();
    pending.bytes.reserve(2 * protocol::MAX_FRAME_SIZE);
    assert!(crate::server::feedback_delivery::PendingFeedback::new(
        pending.capture,
        pending.prepared,
        pending.bytes,
        pending.trace,
        pending.receipts
    )
    .is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
#[ignore = "manual pending delivery scaling profile"]
async fn pending_feedback_render_scale_profile() {
    for panes in [1, 15] {
        for clients in [1, 3] {
            let mut server = test_headless_server();
            let pane = install_shared_view_test_runtime(&mut server);
            let mut ids = vec![pane];
            for n in 1..panes {
                server.app.state.workspaces[0].tabs[0]
                    .layout
                    .focus_pane(ids[(n - 1) / 2]);
                let direction = if n % 2 == 0 {
                    ratatui::layout::Direction::Vertical
                } else {
                    ratatui::layout::Direction::Horizontal
                };
                let id = server.app.state.workspaces[0].test_split(direction);
                server.app.state.workspaces[0].insert_test_runtime(
                    id,
                    crate::terminal::TerminalRuntime::test_with_screen_bytes(120, 40, b"POPULATED"),
                );
                ids.push(id);
            }
            server.app.state.workspaces[0].tabs[0]
                .layout
                .focus_pane(pane);
            let mut channels = Vec::new();
            for id in 1..=clients {
                channels.push(connect_test_shell(&mut server, id, 240, 80));
            }
            for (control, _) in &channels {
                let _ = control.recv().unwrap();
            }
            server.render_and_stream();
            for (_, render) in &channels {
                let _ = render.recv().unwrap();
            }
            let mut writers = Vec::new();
            for id in 1..=clients {
                let writer = ClientWriter::test_paused();
                server.clients.get_mut(&id).unwrap().writer = Some(writer.clone());
                writers.push(writer);
            }
            let instance = server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
                .unwrap()
                .runtime_instance();
            let mut samples = Vec::new();
            for n in 0..100 {
                for writer in &writers {
                    writer
                        .render
                        .send_ordered(
                            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
                                .unwrap(),
                        )
                        .unwrap();
                }
                write_shared_test_pane(&mut server, pane, format!("\rVALUE{n:03}").as_bytes());
                let start = Instant::now();
                server.render_target_pane_surface_traced(
                    pane,
                    instance,
                    crate::latency_prof::TraceContext::default(),
                );
                for (i, writer) in writers.iter().enumerate() {
                    assert!(server.clients[&(i as u64 + 1)].pending_feedback.is_some());
                    let _ = writer.test_drain();
                    assert!(
                        !server.handle_server_event(ServerEvent::ClientWriterDrained {
                            client_id: i as u64 + 1
                        })
                    );
                    assert_eq!(writer.test_drain().len(), 1);
                }
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            println!(
                "pending_delivery panes={panes} clients={clients} median_ns={} p95_ns={}",
                samples[50], samples[94]
            );
            shutdown_test_runtimes(&mut server);
        }
    }
}

#[tokio::test]
async fn pending_feedback_synchronized_recovery_waits_for_end_without_losing_output() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    write_shared_test_pane(&mut server, pane, b"\x1b[?2026h");
    let _ = writer.test_drain();
    assert!(!server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 }));
    assert!(server.feedback_recovery.contains(&27));
    assert!(
        !server.has_ready_feedback_recovery(),
        "producer owns wake during synchronization"
    );
    server.render_feedback_recovery_traced(crate::latency_prof::TraceContext::default());
    assert!(writer.test_drain().is_empty());
    assert!(server.feedback_recovery.contains(&27));
    write_shared_test_pane(&mut server, pane, b"\x1b[?2026l");
    assert!(server.has_ready_feedback_recovery());
    server.render_feedback_recovery_traced(crate::latency_prof::TraceContext::default());
    assert_eq!(writer.test_drain().len(), 1);
    assert!(server.clients[&27]
        .render_state
        .last_pane_surface()
        .unwrap()
        .frame
        .cells
        .iter()
        .map(|c| c.symbol.as_str())
        .collect::<String>()
        .contains("ECHO"));
    assert!(!server.feedback_recovery.contains(&27));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_unchanged_recovery_clears_scheduling() {
    let mut server = test_headless_server();
    let _ = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    let client = server.clients.get_mut(&27).unwrap();
    client.defer_full_render();
    client.feedback_recovery = true;
    server.feedback_recovery.insert(27);
    server.render_feedback_recovery_traced(crate::latency_prof::TraceContext::default());
    assert!(!server.feedback_recovery.contains(&27));
    assert!(!server.has_ready_feedback_recovery());
    assert!(writer.test_drain().is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_zoomed_recovery_ignores_hidden_synchronization() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let hidden = server.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    server.app.state.workspaces[0].insert_test_runtime(
        hidden,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"HIDDEN"),
    );
    server.app.state.workspaces[0].tabs[0]
        .layout
        .focus_pane(pane);
    server.app.state.workspaces[0].tabs[0].zoomed = true;
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    write_shared_test_pane(&mut server, hidden, b"\x1b[?2026h");
    let client = server.clients.get_mut(&27).unwrap();
    client.defer_full_render();
    client.feedback_recovery = true;
    server.feedback_recovery.insert(27);
    assert!(server.has_ready_feedback_recovery());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_no_output_input_does_not_construct_or_schedule() {
    use crate::app::early_presentation::{EarlyPresentation, TerminalPlacement};
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let terminal = server.app.state.workspaces[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let runtime = server.app.state.workspaces[0]
        .test_runtimes
        .remove(&pane)
        .unwrap();
    let instance = runtime.runtime_instance();
    let revision = runtime.content_seq();
    server
        .app
        .terminal_runtimes
        .insert(terminal.clone(), runtime);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    server.app.render_dirty.take_pending().complete();
    server.app.early_presentation =
        EarlyPresentation::new(crate::latency_experiments::PresentationPolicy::ActionFullTarget);
    let now = Instant::now();
    server
        .app
        .early_presentation
        .accepted_terminal(
            terminal,
            TerminalPlacement {
                workspace_id: server.app.state.workspaces[0].id.clone(),
                tab_root: pane,
                pane_id: pane,
                public_pane_id: server.app.public_pane_id(0, pane).unwrap(),
            },
            instance,
            revision,
            None,
            now,
        )
        .unwrap();
    assert!(!server.try_early_terminal_feedback(now));
    assert!(server.clients[&27].pending_feedback.is_none());
    assert!(!server.has_ready_feedback_recovery());
    assert!(render.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_popup_synchronization_sleeps_until_end() {
    let mut server = test_headless_server();
    let _ = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let (_, terminal) = server.app.install_test_popup_runtime(
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"POPUP"),
    );
    server.popup_owner_tab_id = server.shell_tab_id_for_client(27);
    server
        .app
        .terminal_runtimes
        .get(&terminal)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?2026h");
    server.feedback_recovery.insert(27);
    assert!(!server.has_ready_feedback_recovery());
    server
        .app
        .terminal_runtimes
        .get(&terminal)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?2026l");
    assert!(server.has_ready_feedback_recovery());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_feedback_full_request_waits_for_drain_without_polling() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    let instance = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .runtime_instance();
    server.render_target_pane_surface_traced(
        pane,
        instance,
        crate::latency_prof::TraceContext::default(),
    );
    let baseline = server.clients[&27]
        .render_state
        .last_pane_surface()
        .unwrap()
        .clone();
    server.app.full_redraw_pending = true;
    server.render_and_stream();
    assert!(server.clients[&27].pending_feedback.is_some());
    assert_eq!(
        server.clients[&27].render_state.last_pane_surface(),
        Some(&baseline)
    );
    assert!(
        !server.app.full_redraw_pending,
        "recipient owns deferred request, writer drain owns wake"
    );
    shutdown_test_runtimes(&mut server);
}

fn feedback_burst_fixture() -> (
    HeadlessServer,
    crate::layout::PaneId,
    u64,
    crate::app::early_presentation::Opportunity,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    use crate::app::early_presentation::{EarlyPresentation, TerminalPlacement};
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let terminal = server.app.state.workspaces[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let runtime = server.app.state.workspaces[0]
        .test_runtimes
        .remove(&pane)
        .unwrap();
    server.app.terminal_runtimes.insert(terminal, runtime);
    let (control, render) = connect_matching_test_shell(&mut server, 27);
    let _ = control.recv().unwrap();
    server.render_and_stream();
    let _ = render.recv().unwrap();
    server.app.early_presentation =
        EarlyPresentation::new(crate::latency_experiments::PresentationPolicy::Target);
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap();
    let instance = runtime.runtime_instance();
    let revision = runtime.content_seq();
    let now = Instant::now();
    let grant = server
        .app
        .early_presentation
        .accepted_terminal(
            server.app.state.workspaces[0]
                .terminal_id(pane)
                .unwrap()
                .clone(),
            TerminalPlacement {
                workspace_id: server.app.state.workspaces[0].id.clone(),
                tab_root: pane,
                pane_id: pane,
                public_pane_id: server.app.public_pane_id(0, pane).unwrap(),
            },
            instance,
            revision,
            None,
            now,
        )
        .unwrap();
    server
        .app
        .render_dirty
        .arm_target_ready(pane, instance, revision, grant.expires_at, grant.id);
    server.app.render_dirty.take_pending().complete();
    (server, pane, instance, grant, render)
}

#[tokio::test]
async fn feedback_burst_presents_echo_after_older_first_output() {
    let (mut server, pane, instance, grant, render) = feedback_burst_fixture();
    let now = grant.accepted_at;
    server.app.render_dirty.take_pending().complete();
    write_shared_test_pane(&mut server, pane, b"\rOLDER");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    server.try_early_terminal_feedback(now);
    let _ = render.recv().unwrap();
    write_shared_test_pane(&mut server, pane, b"\rECHO ");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    server.try_early_terminal_feedback(now + Duration::from_millis(1));
    assert!(
        render.try_recv().is_ok(),
        "echo must use the retained follow-up before cadence"
    );
    assert!(!server
        .app
        .early_presentation
        .admit(grant.id, now + Duration::from_millis(2), false));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn feedback_burst_unchanged_output_spends_attempt_without_repeat() {
    let (mut server, pane, instance, grant, render) = feedback_burst_fixture();
    write_shared_test_pane(&mut server, pane, b"\rBASE");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    server.try_early_terminal_feedback(grant.accepted_at);
    assert!(
        render.try_recv().is_err(),
        "unchanged cells cannot emit feedback"
    );
    assert!(server
        .app
        .early_presentation
        .oldest_ready(grant.accepted_at, |_| true)
        .is_none());
    write_shared_test_pane(&mut server, pane, b"\rLATER");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    for _ in 0..10 {
        server.try_early_terminal_feedback(grant.accepted_at);
    }
    assert!(
        render.try_recv().is_err(),
        "unsatisfied construction cannot repeat"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn feedback_burst_blocked_recipient_cannot_spend_construction() {
    let (mut server, pane, instance, grant, render) = feedback_burst_fixture();
    server.clients.get_mut(&27).unwrap().defer_full_render();
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    for _ in 0..10 {
        server.try_early_terminal_feedback(grant.accepted_at);
    }
    assert!(render.try_recv().is_err());
    assert!(
        server
            .app
            .early_presentation
            .admit(grant.id, grant.accepted_at, false),
        "all-blocked recipients must preserve construction budget"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn feedback_burst_saved_first_delivery_retains_echo_and_budget() {
    let (mut server, pane, instance, grant, _render) = feedback_burst_fixture();
    let writer = ClientWriter::test_paused();
    server.clients.get_mut(&27).unwrap().writer = Some(writer.clone());
    writer
        .render
        .send_ordered(
            HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig).unwrap(),
        )
        .unwrap();
    write_shared_test_pane(&mut server, pane, b"\rOLDER");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    server.try_early_terminal_feedback(grant.accepted_at);
    assert!(server.clients[&27].pending_feedback.is_some());
    write_shared_test_pane(&mut server, pane, b"\rECHO");
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    assert!(!server
        .app
        .early_presentation
        .admit(grant.id, grant.accepted_at, false));
    writer.test_drain();
    server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 27 });
    assert_eq!(
        writer.test_drain().len(),
        1,
        "retry reuses saved first patch"
    );
    assert!(
        server.feedback_recovery.contains(&27),
        "newer echo retains recipient recovery"
    );
    assert!(server.app.render_dirty.has_pending_source(pane));
    assert!(
        server.app.early_presentation.admit(
            grant.id,
            grant.accepted_at + Duration::from_millis(1),
            false
        ),
        "retry costs no second construction"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn feedback_burst_graphics_output_keeps_ordinary_owner() {
    let (mut server, pane, instance, grant, render) = feedback_burst_fixture();
    write_shared_test_pane(
        &mut server,
        pane,
        b"\x1b_Ga=T,f=32,s=1,v=1,i=41;/////w==\x1b\\",
    );
    assert!(server
        .app
        .terminal_runtimes
        .values()
        .next()
        .unwrap()
        .graphics_may_have_placements());
    server.app.render_dirty.request_pty_ready(
        pane,
        instance,
        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .content_seq(),
    );
    server.try_early_terminal_feedback(grant.accepted_at);
    assert!(render.try_recv().is_err());
    assert!(
        server
            .app
            .early_presentation
            .admit(grant.id, grant.accepted_at, false),
        "graphics must not spend selected text budget"
    );
    shutdown_test_runtimes(&mut server);
}
