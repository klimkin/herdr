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
