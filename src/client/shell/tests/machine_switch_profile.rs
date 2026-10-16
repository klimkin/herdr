use super::*;
use ratatui::style::Color;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::time::Instant;

const COLS: u16 = 180;
const ROWS: u16 = 64;
const TARGET: &str = "remote_1";
const MARKER: &str = "VIEW-remote_1";
const REPEATS: usize = 5;

struct CapturedTerminal {
    file: File,
    stdout: File,
    offset: u64,
    terminal: ghostty_vt::Terminal,
    bytes: usize,
}

impl CapturedTerminal {
    fn new() -> Self {
        io::stdout().flush().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-machine-switch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        // SAFETY: dup creates an owned descriptor; dup2 replaces stdout with the live file.
        let stdout = unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            assert!(saved >= 0, "stdout duplication failed");
            assert!(libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) >= 0);
            File::from_raw_fd(saved)
        };
        Self {
            file,
            stdout,
            offset: 0,
            terminal: ghostty_vt::Terminal::new(COLS, ROWS, 0).unwrap(),
            bytes: 0,
        }
    }

    fn observe(&mut self) -> String {
        io::stdout().flush().unwrap();
        let mut bytes = [0; 64 * 1024];
        loop {
            let count = self.file.read_at(&mut bytes, self.offset).unwrap();
            if count == 0 {
                break;
            }
            self.offset += count as u64;
            self.bytes += count;
            assert!(self.bytes < 8 * 1024 * 1024, "unbounded terminal output");
            self.terminal.write(&bytes[..count]);
        }
        assert!(
            !self.terminal.mode_get(2026).unwrap(),
            "terminal frame lacks synchronized-output commit"
        );
        self.terminal
            .read_text_viewport((0, 0), (COLS - 1, u32::from(ROWS - 1)), true)
            .unwrap()
    }

    fn target_highlighted(&self, text: &str, color: Color) -> bool {
        let Some((y, line)) = text.lines().enumerate().find(|(_, line)| {
            line.find(TARGET)
                .is_some_and(|target| line.find(MARKER).is_none_or(|marker| target < marker))
        }) else {
            return false;
        };
        let x = line[..line.find(TARGET).unwrap()].chars().count() as u16;
        let mut state = ghostty_vt::RenderState::new().unwrap();
        state.update(&self.terminal).unwrap();
        let mut rows = ghostty_vt::RowIterator::new().unwrap();
        let mut row = state.populate_row_iterator(&mut rows).unwrap();
        for _ in 0..=y {
            if !row.next() {
                return false;
            }
        }
        let mut cells = ghostty_vt::RowCells::new().unwrap();
        let mut cell = row.populate_cells(&mut cells).unwrap();
        cell.select(x).unwrap();
        let expected = match color {
            Color::Rgb(r, g, b) => ghostty_vt::CellColor::Rgb(ghostty_vt::RgbColor { r, g, b }),
            Color::Indexed(index) => ghostty_vt::CellColor::Palette(index),
            _ => panic!("profile requires RGB or indexed selection color"),
        };
        cell.basic_data().unwrap().style.bg_color == Some(expected)
    }
}

impl Drop for CapturedTerminal {
    fn drop(&mut self) {
        let _ = io::stdout().flush();
        // SAFETY: the saved descriptor stays valid until stdout has been restored.
        assert!(unsafe { libc::dup2(self.stdout.as_raw_fd(), libc::STDOUT_FILENO) } >= 0);
    }
}

struct SimulatedTransport {
    now_ms: u64,
    rtt_ms: u64,
    due: [Option<u64>; 2],
    batches: usize,
    errors: usize,
    panes: usize,
}

impl SimulatedTransport {
    fn new(rtt_ms: u64, panes: usize) -> Self {
        Self {
            now_ms: 0,
            rtt_ms,
            due: [None, None],
            batches: 0,
            errors: 0,
            panes,
        }
    }

    fn step(&mut self, harness: &mut Harness) -> bool {
        for (index, server) in [&harness.local, &harness.remote].into_iter().enumerate() {
            if self.due[index].is_none() && server.inbox.lock().unwrap().len() > server.read {
                self.due[index] = Some(self.now_ms + self.rtt_ms);
            }
        }
        let Some(next) = self.due.iter().flatten().copied().min() else {
            return false;
        };
        self.now_ms = next;
        let mut deliveries = Vec::new();
        for (index, server) in [&mut harness.local, &mut harness.remote]
            .into_iter()
            .enumerate()
        {
            if self.due[index] != Some(next) {
                continue;
            }
            self.due[index] = None;
            self.batches += 1;
            server.pump();
            while let Some(mut event) = server.outbox.pop_front() {
                match &mut event {
                    ServerEvent::Snapshot(snapshot) => populate_snapshot(snapshot, self.panes),
                    ServerEvent::Surface(surface) => populate_surface(surface, self.panes),
                    ServerEvent::Response { data, .. } => {
                        self.errors +=
                            usize::from(serde_json::from_slice::<ErrorResponse>(data).is_ok());
                    }
                    ServerEvent::EffectsReady(_) => {}
                }
                deliveries.push((server.endpoint_id.clone(), server.generation, event));
            }
        }
        for (endpoint, generation, event) in deliveries {
            harness.deliver(&endpoint, generation, event);
            harness.run_scheduled();
            repaint(harness);
        }
        true
    }

    fn settle(&mut self, harness: &mut Harness) {
        for _ in 0..64 {
            if !self.step(harness) {
                assert!(!harness.handoff_in_progress(), "handoff did not finish");
                return;
            }
        }
        panic!("unbounded simulated network work");
    }
}

fn populate_snapshot(snapshot: &mut ClientShellSnapshot, count: usize) {
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.focused)
        .unwrap()
        .clone();
    for index in 1..count {
        let mut extra = pane.clone();
        extra.pane_id = format!("{}:extra:{index}", pane.workspace_id);
        extra.focused = false;
        snapshot.panes.push(extra);
    }
}

fn populate_surface(surface: &mut PaneSurfaceFrame, count: usize) {
    let width = surface.frame.width;
    let height = surface.frame.height;
    let focused = surface.panes[0].pane_id.clone();
    let workspace = focused.strip_suffix(":pane").unwrap();
    let template = surface.panes[0].clone();
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    surface.panes.clear();
    for index in 0..count {
        let y = index as u16 * height / count as u16;
        let bottom = (index as u16 + 1) * height / count as u16;
        let mut pane = template.clone();
        pane.pane_id = if index == 0 {
            focused.clone()
        } else {
            format!("{workspace}:extra:{index}")
        };
        pane.focused = index == 0;
        pane.rect = SurfaceRect {
            x: 0,
            y,
            width,
            height: bottom - y,
        };
        pane.inner_rect = pane.rect;
        for row in y..bottom {
            buffer.set_string(
                0,
                row,
                format!(
                    "VIEW-{workspace} pane={index:02} row={row:02} {}",
                    "output ".repeat(12)
                ),
                Style::default(),
            );
        }
        surface.panes.push(pane);
    }
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
}

fn populated_harness(panes: usize) -> Harness {
    let mut harness = Harness::new();
    harness.state.reported_size = (COLS, ROWS);
    let geometry = harness.shell().surface_size(COLS, ROWS);
    harness.local.geometry = geometry;
    harness.remote.geometry = geometry;
    let mut local_snapshot = harness.local.snapshot();
    let mut remote_snapshot = harness.remote.snapshot();
    populate_snapshot(&mut local_snapshot, panes);
    populate_snapshot(&mut remote_snapshot, panes);
    let mut local_surface = harness.local.surface();
    populate_surface(&mut local_surface, panes);
    let shell = harness.state.shell.as_mut().unwrap();
    shell.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        1,
        Box::new(local_snapshot),
    );
    shell.set_pane_surface(local_surface);
    shell.cache_endpoint_snapshot_inactive_for_generation(
        &harness.remote.endpoint_id,
        7,
        Box::new(remote_snapshot),
    );
    repaint(&mut harness);
    harness
}

fn repaint(harness: &mut Harness) {
    let frame = harness.state.shell.as_mut().unwrap().compose(COLS, ROWS);
    if let Some(frame) = frame {
        harness.state.present_frame(frame);
    }
}

#[derive(Debug)]
struct Sample {
    selection_ms: u64,
    text_ms: u64,
    coherent_ms: u64,
    input_ms: u64,
    batches: usize,
    requests: usize,
    fences: usize,
    errors: usize,
    output_bytes: usize,
    compose_encode_us: u64,
}

fn sample(rtt_ms: u64, panes: usize, warm: bool) -> Sample {
    let mut terminal = CapturedTerminal::new();
    let mut harness = populated_harness(panes);
    if warm {
        harness.select_pane(harness.remote_id(), "remote_1:pane");
        SimulatedTransport::new(rtt_ms, panes).settle(&mut harness);
        harness.select_pane(ClientEndpointId::Local, "local_2:pane");
        SimulatedTransport::new(rtt_ms, panes).settle(&mut harness);
    }
    terminal.observe();
    let baseline_bytes = terminal.bytes;
    let starts = [
        harness.local.inbox.lock().unwrap().len(),
        harness.remote.inbox.lock().unwrap().len(),
    ];
    let remote_focus_start = harness.remote.pane_focus_requests().len();
    let mut transport = SimulatedTransport::new(rtt_ms, panes);
    let mut first_selection = None;
    let mut first_text = None;
    let mut first_coherent = None;
    let mut first_input = None;
    harness.press_action(crate::input::KeybindAction::NextAgent);
    for _ in 0..64 {
        repaint(&mut harness);
        let text = terminal.observe();
        let palette = &harness.shell().config.palette;
        if terminal.target_highlighted(&text, palette.active_row_bg)
            || (palette.selection_bg != Color::Reset
                && terminal.target_highlighted(&text, palette.selection_bg))
        {
            first_selection.get_or_insert(transport.now_ms);
        }
        if text.contains(MARKER) {
            first_text.get_or_insert(transport.now_ms);
        }
        let shell = harness.shell();
        let coherent = shell.active_endpoint_id == harness.remote_id()
            && shell.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.boot_id == harness.remote.boot_id
                    && snapshot.focused_pane_id.as_deref() == Some("remote_1:pane")
                    && shell.pane_surface.as_ref().is_some_and(|surface| {
                        surface.projection_revision == snapshot.revision
                            && surface.boot_id == snapshot.boot_id
                            && shell.pane_surface_generation == shell.active_snapshot_generation
                    })
            })
            && text.contains(MARKER);
        if coherent {
            first_coherent.get_or_insert(transport.now_ms);
        }
        if coherent
            && harness.endpoints.active_surface_available()
            && !harness.state.presentation_frozen
        {
            first_input.get_or_insert(transport.now_ms);
        }
        if !transport.step(&mut harness) {
            break;
        }
    }
    assert!(!harness.handoff_in_progress());
    assert_eq!(harness.endpoints.active_id(), &harness.remote_id());
    assert_eq!(harness.remote.focused, TARGET);
    let mut requests = 0;
    let mut fences = 0;
    for (server, start) in [&harness.local, &harness.remote].into_iter().zip(starts) {
        for message in &server.inbox.lock().unwrap()[start..] {
            requests += usize::from(matches!(
                message,
                ClientMessage::ClientShellEndpointRequest { .. }
            ));
            fences += usize::from(
                matches!(message, ClientMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::endpoint::PRESENTATION_EFFECTS_SYNC_KIND),
            );
        }
    }
    assert_eq!(requests, 4, "timed switch changed endpoint request count");
    assert_eq!(fences, 1, "timed switch changed effects fence count");
    assert_eq!(transport.batches, 4, "timed switch changed network batches");
    assert_eq!(
        &harness.remote.pane_focus_requests()[remote_focus_start..],
        &[String::from("remote_1:pane")],
        "timed switch did not request target pane focus"
    );
    let errors = transport.errors + usize::from(harness.shell().endpoint_error.is_some());
    assert_eq!(errors, 0, "profile encountered endpoint errors");
    let output_bytes = terminal.bytes - baseline_bytes;
    assert!(output_bytes > 0, "timed switch produced no terminal output");
    let mut costs = Vec::new();
    for _ in 0..25 {
        let started = Instant::now();
        let frame = harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .compose(COLS, ROWS)
            .unwrap();
        let mut output = Vec::new();
        assert!(harness.state.try_present_frame_to(&mut output, frame));
        costs.push(started.elapsed().as_micros() as u64);
        std::hint::black_box(output);
    }
    costs.sort_unstable();
    Sample {
        selection_ms: first_selection.expect("target selection was never displayed"),
        text_ms: first_text.expect("target text was never displayed"),
        coherent_ms: first_coherent.expect("target projection never became coherent"),
        input_ms: first_input.expect("target input never became available"),
        batches: transport.batches,
        requests,
        fences,
        errors,
        output_bytes,
        compose_encode_us: costs[costs.len() / 2],
    }
}

fn median_range(samples: &[Sample], field: impl Fn(&Sample) -> u64) -> String {
    let mut values = samples.iter().map(field).collect::<Vec<_>>();
    values.sort_unstable();
    format!(
        "{}[{},{}]",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

#[test]
#[ignore = "release profile with simulated transport; run just bench-machine-switch"]
fn machine_switch_profile() {
    eprintln!(
        "machine-switch-profile simulated_transport=true geometry={COLS}x{ROWS} repeats={REPEATS}"
    );
    eprintln!("simulated_ms assigns equal RTT to both endpoints and omits host CPU and physical network; CPU_us times actual compose+encoder+Vec output, 25 retained frames/sample");
    for panes in [1, 15] {
        for rtt_ms in [0, 80, 200] {
            let mut cold = Vec::new();
            let mut warm = Vec::new();
            for _ in 0..REPEATS {
                cold.push(sample(rtt_ms, panes, false));
                warm.push(sample(rtt_ms, panes, true));
            }
            for (cache, samples) in [("cold", cold), ("warm", warm)] {
                eprintln!(
                    "panes={panes} cache={cache} both_endpoint_rtt_ms={rtt_ms} selection_ms={} text_ms={} coherent_ms={} input_ms={} compose_encode_us={} batches={:?} requests={:?} fences={:?} errors={} output_bytes={:?} final_focus=remote_1:pane",
                    median_range(&samples, |sample| sample.selection_ms),
                    median_range(&samples, |sample| sample.text_ms),
                    median_range(&samples, |sample| sample.coherent_ms),
                    median_range(&samples, |sample| sample.input_ms),
                    median_range(&samples, |sample| sample.compose_encode_us),
                    samples.iter().map(|sample| sample.batches).collect::<Vec<_>>(),
                    samples.iter().map(|sample| sample.requests).collect::<Vec<_>>(),
                    samples.iter().map(|sample| sample.fences).collect::<Vec<_>>(),
                    samples.iter().map(|sample| sample.errors).sum::<usize>(),
                    samples.iter().map(|sample| sample.output_bytes).collect::<Vec<_>>(),
                );
            }
        }
    }
}

#[derive(Debug)]
struct PreviewCost {
    capture_us: u64,
    preview_us: u64,
    total_us: u64,
    output_bytes: usize,
}

fn preview_cost_sample(panes: usize) -> PreviewCost {
    let _capture = CapturedTerminal::new();
    let mut harness = populated_harness(panes);
    harness.select_pane(harness.remote_id(), "remote_1:pane");
    SimulatedTransport::new(0, panes).settle(&mut harness);
    harness.select_pane(ClientEndpointId::Local, "local_2:pane");
    SimulatedTransport::new(0, panes).settle(&mut harness);
    let mut terminal = ghostty_vt::Terminal::new(COLS, ROWS, 0).unwrap();
    let mut captures = Vec::new();
    let mut previews = Vec::new();
    let mut totals = Vec::new();
    let mut output_bytes = 0;
    for _ in 0..25 {
        harness.state.presentation_frozen = false;
        harness.state.request_repaint();
        harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .clear_pending_preview();
        let source = harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .compose(COLS, ROWS)
            .unwrap();
        let mut source_output = Vec::new();
        assert!(harness
            .state
            .try_present_frame_to(&mut source_output, source));
        terminal.write(&source_output);
        let remote_id = harness.remote_id();
        let started = Instant::now();
        harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .remember_text_preview(COLS, ROWS);
        let captured_us = started.elapsed().as_micros() as u64;
        harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .select_pending_preview(
                &remote_id,
                Some(7),
                Some(ClientEndpointFocusTarget::Pane("remote_1:pane".into())),
            );
        harness.state.presentation_frozen = true;
        let preview = harness
            .state
            .shell
            .as_mut()
            .unwrap()
            .compose(COLS, ROWS)
            .unwrap();
        assert_eq!(
            preview.presentation,
            crate::client::frame_output::FramePresentation::Preview
        );
        let mut output = Vec::new();
        assert!(harness.state.try_present_frame_to(&mut output, preview));
        let total_us = started.elapsed().as_micros() as u64;
        captures.push(captured_us);
        previews.push(total_us - captured_us);
        totals.push(total_us);
        assert!(!output.is_empty(), "preview produced no terminal output");
        output_bytes += output.len();
        terminal.write(&output);
        assert!(
            !terminal.mode_get(2026).unwrap(),
            "preview output did not commit"
        );
        let text = terminal
            .read_text_viewport((0, 0), (COLS - 1, u32::from(ROWS - 1)), true)
            .unwrap();
        assert!(text.contains(MARKER), "preview cache missed target text");
        assert_eq!(harness.endpoints.active_id(), &ClientEndpointId::Local);
        assert!(!harness.handoff_in_progress());
    }
    captures.sort_unstable();
    previews.sort_unstable();
    totals.sort_unstable();
    PreviewCost {
        capture_us: captures[captures.len() / 2],
        preview_us: previews[previews.len() / 2],
        total_us: totals[totals.len() / 2],
        output_bytes,
    }
}

fn preview_cost_range(samples: &[PreviewCost], field: impl Fn(&PreviewCost) -> u64) -> String {
    let mut values = samples.iter().map(field).collect::<Vec<_>>();
    values.sort_unstable();
    format!(
        "{}[{},{}]",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

#[test]
#[ignore = "candidate-only preview CPU profile; run just bench-machine-switch"]
fn machine_preview_cost_profile() {
    eprintln!("machine-preview-cost-profile candidate_only=true geometry={COLS}x{ROWS} repeats={REPEATS} iterations=25 cache=warm output=Vec");
    eprintln!("CPU_us measures outgoing text capture plus cached target selection, composition and encoder; excludes setup, transport and terminal observer");
    for panes in [1, 15] {
        let samples = (0..REPEATS)
            .map(|_| preview_cost_sample(panes))
            .collect::<Vec<_>>();
        eprintln!(
            "panes={panes} capture_us={} preview_compose_encode_us={} total_us={} verified_previews={} errors=0 output_bytes={:?}",
            preview_cost_range(&samples, |sample| sample.capture_us),
            preview_cost_range(&samples, |sample| sample.preview_us),
            preview_cost_range(&samples, |sample| sample.total_us),
            REPEATS * 25,
            samples.iter().map(|sample| sample.output_bytes).collect::<Vec<_>>(),
        );
    }
}
