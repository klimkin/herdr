use super::*;
use std::{hint::black_box, time::Instant};

#[tokio::test(flavor = "current_thread")]
#[ignore = "manual retained preparation scaling profile"]
async fn retained_frame_cost_profile() {
    for count in [1, 15] {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("retained-cost");
        let history: String = (0..100).map(|_| "history\r\n").collect();
        let root = workspace.focused_pane_id().expect("root pane");
        workspace.insert_test_runtime(
            root,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                120,
                40,
                100_000,
                history.as_bytes(),
            ),
        );
        let mut panes = vec![root];
        for index in 1..count {
            workspace.tabs[0].layout.focus_pane(panes[(index - 1) / 2]);
            let pane = workspace.test_split(if index % 2 == 0 {
                ratatui::layout::Direction::Vertical
            } else {
                ratatui::layout::Direction::Horizontal
            });
            workspace.insert_test_runtime(
                pane,
                crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                    120,
                    40,
                    100_000,
                    history.as_bytes(),
                ),
            );
            panes.push(pane);
        }
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let receivers: Vec<_> = (1..=3)
            .map(|id| connect_test_shell(&mut server, id, 120, 40))
            .collect();
        server.render_and_stream();
        for (_, render) in &receivers {
            render.recv().expect("initial surface");
        }
        let sources = panes.iter().copied().collect();
        let mut samples = Vec::new();
        for i in 0..1050 {
            for pane in &panes {
                write_shared_test_pane(
                    &mut server,
                    *pane,
                    format!("\x1b[H{i:08}\x1b[K").as_bytes(),
                );
            }
            let started = Instant::now();
            assert!(server.render_retained_pane_surface_and_stream(&sources));
            let elapsed = started.elapsed();
            for (_, render) in &receivers {
                black_box(render.recv().expect("retained patch"));
            }
            if i >= 50 {
                samples.push(elapsed.as_nanos());
            }
        }
        samples.sort_unstable();
        println!("retained_cost panes={count} clients=3 geometry=120x40 samples={} median_ns={} p95_ns={} p99_ns={} max_ns={}", samples.len(), samples[500], samples[949], samples[989], samples[999]);
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn retained_cursor_matches_full_render_for_each_recipient() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    let receivers: Vec<_> = [7, 8, 9]
        .into_iter()
        .map(|id| connect_matching_test_shell(&mut server, id))
        .collect();
    server.render_and_stream();
    for (_, render) in &receivers {
        render.recv().expect("initial frame");
    }
    for bytes in [
        b"\x1b[3;5H\x1b[5 q".as_slice(),
        b"\x1b[?25l".as_slice(),
        b"\x1b[?25h\x1b[2;4H".as_slice(),
    ] {
        write_shared_test_pane(&mut server, pane, bytes);
        let reads_before = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
            .unwrap()
            .test_cursor_reads();
        assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane])));
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
                .unwrap()
                .test_cursor_reads(),
            reads_before,
            "collected cursor must avoid per-client reads"
        );
        let cursors: Vec<_> = receivers
            .iter()
            .map(|(_, render)| recv_pane_surface_patch(render, "cursor patch").cursor)
            .collect();
        for id in [7, 8, 9] {
            server.clients.get_mut(&id).unwrap().request_recompute();
        }
        server.render_and_stream();
        for ((_, render), cursor) in receivers.iter().zip(cursors) {
            let full = recv_pane_surface(render, "full cursor frame");
            assert_eq!(cursor, full.frame.cursor);
        }
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_cursor_reads_focused_pane_outside_dirty_sources() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("unselected-cursor");
    let background = workspace.focused_pane_id().unwrap();
    workspace.insert_test_runtime(
        background,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(120, 40, b"background"),
    );
    let focused = workspace.test_split(ratatui::layout::Direction::Horizontal);
    workspace.insert_test_runtime(
        focused,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(120, 40, b"focused"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let (_, render) = connect_test_shell(&mut server, 7, 120, 40);
    server.render_and_stream();
    render.recv().unwrap();
    write_shared_test_pane(&mut server, focused, b"\x1b[3;4H");
    let before = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, focused)
        .unwrap()
        .test_cursor_reads();
    write_shared_test_pane(&mut server, background, b"\rchanged");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([background])));
    let cursor = recv_pane_surface_patch(&render, "background update").cursor;
    assert!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, focused)
            .unwrap()
            .test_cursor_reads()
            > before
    );
    server.clients.get_mut(&7).unwrap().request_recompute();
    server.render_and_stream();
    assert_eq!(
        cursor,
        recv_pane_surface(&render, "full cursor").frame.cursor
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_captured_absent_cursor_does_not_read_live_cursor() {
    let mut server = test_headless_server();
    let pane = install_shared_view_test_runtime(&mut server);
    server.app.state.workspaces[0].insert_test_runtime(
        pane,
        crate::terminal::TerminalRuntime::test_with_scrollback_bytes(80, 23, 100_000, b"BASE"),
    );
    let (_, render) = connect_matching_test_shell(&mut server, 7);
    server.render_and_stream();
    render.recv().unwrap();
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap();
    let history: String = (0..40).map(|_| "history\r\n").collect();
    runtime.test_process_pty_bytes(history.as_bytes());
    runtime.scroll_up(5);
    let reads = runtime.test_cursor_reads();
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane])));
    let patch = recv_pane_surface_patch(&render, "scrolled update");
    assert!(patch.cursor.is_none() || !patch.cursor.unwrap().visible);
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
            .unwrap()
            .test_cursor_reads(),
        reads
    );
    shutdown_test_runtimes(&mut server);
}
