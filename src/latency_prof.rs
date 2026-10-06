//! Opt-in batch diagnostics. No runtime state or published codec is changed.

#[cfg(feature = "latency-prof")]
mod enabled {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Mutex, OnceLock};
    thread_local! {
        static THREAD: u64 = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
    }
    static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);

    #[derive(serde::Serialize)]
    struct Record {
        stage: &'static str,
        ns: u64,
        pid: u32,
        thread: u64,
        id: u64,
        scope: u64,
        value: u64,
        dropped: u64,
        connection: u64,
        occurrence: u64,
        offset: u64,
    }

    struct Profiler {
        sender: mpsc::SyncSender<Command>,
        worker: Mutex<Option<std::thread::JoinHandle<()>>>,
        dropped: AtomicU64,
        tracy: Option<tracy_client::Client>,
    }

    enum Command {
        Record(Record),
        Finish(u64, mpsc::Sender<()>),
    }

    static PROFILER: OnceLock<Option<Profiler>> = OnceLock::new();
    fn profiler() -> Option<&'static Profiler> {
        PROFILER
            .get_or_init(|| {
                let path = PathBuf::from(std::env::var_os("HERDR_LATENCY_TRACE_DIR")?);
                std::fs::create_dir_all(&path).ok()?;
                let file = File::create(path.join(format!("{}.jsonl", std::process::id()))).ok()?;
                let (sender, receiver) = mpsc::sync_channel::<Command>(65536);
                let worker = std::thread::Builder::new()
                    .name("latency-records".into())
                    .spawn(move || {
                        let mut file = BufWriter::new(file);
                        loop {
                            match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                                Ok(Command::Record(record)) => {
                                    if serde_json::to_writer(&mut file, &record).is_err()
                                        || file.write_all(b"\n").is_err() { break; }
                                }
                                Ok(Command::Finish(dropped, done)) => {
                                    let result = writeln!(file,
                                        "{{\"stage\":\"process.finish\",\"pid\":{},\"ns\":{},\"id\":0,\"scope\":0,\"value\":0,\"dropped\":{dropped}}}",
                                        std::process::id(), crate::platform::latency::monotonic_ns().unwrap_or(0))
                                        .and_then(|()| file.flush());
                                    if result.is_ok() { let _ = done.send(()); }
                                    break;
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    if file.flush().is_err() { break; }
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    }).ok()?;
                let tracy = (std::env::var_os("HERDR_TRACY").as_deref()
                    == Some(std::ffi::OsStr::new("1")))
                .then(tracy_client::Client::start);
                Some(Profiler {
                    sender,
                    worker: Mutex::new(Some(worker)),
                    dropped: AtomicU64::new(0),
                    tracy,
                })
            })
            .as_ref()
    }

    pub(super) fn record_at(stage: &'static str, id: u64, value: u64, scope: u64, ns: u64) {
        record_frame_at(stage, id, value, scope, ns, super::FrameIdentity::default());
    }

    pub(super) fn record_frame_at(
        stage: &'static str,
        id: u64,
        value: u64,
        scope: u64,
        ns: u64,
        frame: super::FrameIdentity,
    ) {
        let Some(profiler) = profiler() else {
            return;
        };
        if let Some(client) = &profiler.tracy {
            let boundary = client
                .clone()
                .span(tracy_client::span_location!("latency.boundary"), 0);
            boundary.emit_value(ns);
        }
        let record = Record {
            stage,
            ns,
            pid: std::process::id(),
            thread: THREAD.with(|id| *id),
            id,
            scope,
            value,
            dropped: profiler.dropped.load(Ordering::Relaxed),
            connection: frame.connection,
            occurrence: frame.occurrence,
            offset: frame.offset,
        };
        if profiler.sender.try_send(Command::Record(record)).is_err() {
            profiler.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn shutdown() {
        let Some(profiler) = PROFILER.get().and_then(Option::as_ref) else {
            return;
        };
        let Ok(mut worker) = profiler.worker.lock() else {
            return;
        };
        let Some(handle) = worker.take() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut command = Command::Finish(profiler.dropped.load(Ordering::Relaxed), sender);
        loop {
            match profiler.sender.try_send(command) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) if std::time::Instant::now() < deadline => {
                    command = returned;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(_) => {
                    eprintln!("latency recorder final flush unavailable");
                    return;
                }
            }
        }
        if receiver
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .is_ok()
        {
            let _ = handle.join();
        } else {
            eprintln!("latency recorder final flush timed out; trailing records may be missing");
        }
    }

    pub(super) fn span(
        location: &'static tracy_client::SpanLocation,
    ) -> Option<tracy_client::Span> {
        profiler()?
            .tracy
            .as_ref()
            .map(|client| client.clone().span(location, 0))
    }

    pub(super) fn active() -> bool {
        profiler().is_some()
    }

    pub(super) fn mark(stage: &'static str) {
        let Some(client) = profiler().and_then(|profiler| profiler.tracy.as_ref()) else {
            return;
        };
        match stage {
            "server.publish" => {
                client.secondary_frame_mark(tracy_client::frame_name!("server.publish"))
            }
            "client.output_complete" => {
                client.secondary_frame_mark(tracy_client::frame_name!("client.output"))
            }
            _ => {}
        }
    }

    pub(super) fn plot(name: tracy_client::PlotName, value: u64) {
        if let Some(client) = profiler().and_then(|profiler| profiler.tracy.as_ref()) {
            client.plot(name, value as f64);
        }
    }
}

#[inline]
pub(crate) fn record(stage: &'static str, id: u64, value: u64) {
    #[cfg(feature = "latency-prof")]
    {
        enabled::record_at(stage, id, value, 0, now());
        enabled::mark(stage);
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (stage, id, value);
}

#[cfg(feature = "latency-prof")]
pub(crate) fn span(location: &'static tracy_client::SpanLocation) -> Option<tracy_client::Span> {
    enabled::span(location)
}

/// Static source locations exist only in builds explicitly enabling profiling.
macro_rules! zone {
    ($name:literal) => {
        #[cfg(feature = "latency-prof")]
        let _latency_zone = crate::latency_prof::span(tracy_client::span_location!($name));
    };
}
pub(crate) use zone;

macro_rules! plot {
    ($name:literal, $value:expr) => {
        #[cfg(feature = "latency-prof")]
        crate::latency_prof::plot_value(tracy_client::plot_name!($name), $value as u64);
    };
}
pub(crate) use plot;

#[cfg(feature = "latency-prof")]
pub(crate) fn plot_value(name: tracy_client::PlotName, value: u64) {
    enabled::plot(name, value);
}

/// A diagnostic fingerprint links complete transport batches without wire fields.
#[inline]
pub(crate) fn bytes_id(bytes: &[u8]) -> u64 {
    #[cfg(feature = "latency-prof")]
    {
        if !enabled::active() {
            return 0;
        }
        bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }
    #[cfg(not(feature = "latency-prof"))]
    {
        let _ = bytes;
        0
    }
}

pub(crate) fn frames(stage: &'static str, bytes: &[u8]) {
    #[cfg(feature = "latency-prof")]
    {
        if !enabled::active() {
            return;
        }
        let mut remaining = bytes;
        while let Some(prefix) = remaining.get(..4) {
            let len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
            let Some(payload) = remaining.get(4..4 + len) else {
                break;
            };
            record(stage, bytes_id(payload), len as u64);
            remaining = &remaining[4 + len..];
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (stage, bytes);
}

pub(crate) fn message(msg: &crate::protocol::ServerMessage, stage: &'static str) {
    #[cfg(feature = "latency-prof")]
    match msg {
        crate::protocol::ServerMessage::PaneSurface(surface) => {
            record(stage, surface.surface_revision, surface.projection_revision);
        }
        crate::protocol::ServerMessage::PaneSurfacePatch(patch) => {
            record(stage, patch.surface_revision, patch.base_surface_revision);
        }
        crate::protocol::ServerMessage::Terminal(frame) => {
            record(stage, frame.seq, frame.bytes.len() as u64)
        }
        _ => {}
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (msg, stage);
}

/// Initialize before entering hot paths; guard drains the recorder on normal exit.
pub(crate) struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        shutdown();
    }
}
pub(crate) fn shutdown() {
    #[cfg(feature = "latency-prof")]
    enabled::shutdown();
}
pub(crate) fn now() -> u64 {
    #[cfg(feature = "latency-prof")]
    {
        if enabled::active() {
            crate::platform::latency::monotonic_ns().unwrap_or(0)
        } else {
            0
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    {
        0
    }
}
pub(crate) fn record_at(stage: &'static str, id: u64, value: u64, scope: u64, ns: u64) {
    #[cfg(feature = "latency-prof")]
    enabled::record_at(stage, id, value, scope, ns);
    #[cfg(not(feature = "latency-prof"))]
    let _ = (stage, id, value, scope, ns);
}
#[cfg(feature = "latency-prof")]
pub(crate) fn next_scope() -> u64 {
    #[cfg(feature = "latency-prof")]
    {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "latency-prof"))]
    {
        0
    }
}

/// Hash outside queue locks; emission can retain the precise enqueue timestamp.
#[cfg(feature = "latency-prof")]
pub(crate) fn frame_ids(bytes: &[u8]) -> Vec<(u64, u64)> {
    if !enabled::active() {
        return Vec::new();
    }
    let mut ids = Vec::new();
    let mut remaining = bytes;
    while let Some(prefix) = remaining.get(..4) {
        let len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
        let Some(payload) = remaining.get(4..4 + len) else {
            break;
        };
        ids.push((bytes_id(payload), len as u64));
        remaining = &remaining[4 + len..];
    }
    ids
}

#[cfg(feature = "latency-prof")]
thread_local! {
    static WIRE_FRAME: std::cell::Cell<FrameIdentity> = const { std::cell::Cell::new(FrameIdentity::empty()) };
    static READ_POSITION: std::cell::Cell<(u64,u64)> = const { std::cell::Cell::new((0,0)) };
    static DELIVERY_FRAME: std::cell::Cell<FrameIdentity> = const { std::cell::Cell::new(FrameIdentity::empty()) };
}
pub(crate) fn wire_received(bytes: &[u8]) {
    #[cfg(feature = "latency-prof")]
    {
        if !enabled::active() {
            return;
        }
        let id = bytes_id(bytes);
        let frame = READ_POSITION.with(|position| {
            let (connection, offset) = position.get();
            position.set((connection, offset.saturating_add(bytes.len() as u64 + 4)));
            FrameIdentity {
                fingerprint: id,
                connection,
                occurrence: 0,
                offset,
            }
        });
        WIRE_FRAME.with(|value| value.set(frame));
        record_frame_at("transport.receive", bytes.len() as u64, 0, now(), frame);
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = bytes;
}
#[cfg(feature = "latency-prof")]
pub(crate) fn wire_id() -> u64 {
    wire_frame().fingerprint
}
#[cfg(feature = "latency-prof")]
pub(crate) fn wire_frame() -> FrameIdentity {
    WIRE_FRAME.with(std::cell::Cell::get)
}
pub(crate) fn set_delivery(frame: FrameIdentity) {
    #[cfg(feature = "latency-prof")]
    DELIVERY_FRAME.with(|value| value.set(frame));
    #[cfg(not(feature = "latency-prof"))]
    let _ = frame;
}
pub(crate) fn delivery(bytes: usize) {
    #[cfg(feature = "latency-prof")]
    record_frame_at(
        "client.delivery",
        bytes as u64,
        0,
        now(),
        DELIVERY_FRAME.with(std::cell::Cell::get),
    );
    #[cfg(not(feature = "latency-prof"))]
    let _ = bytes;
}

/// Controlled identities are parsed before terminal locks, across PTY batches.
#[derive(Default)]
pub(crate) struct Stimuli {
    #[cfg(feature = "latency-prof")]
    tail: Vec<u8>,
}
impl Stimuli {
    pub(crate) fn observe(&mut self, bytes: &[u8]) -> Vec<u64> {
        #[cfg(feature = "latency-prof")]
        {
            if !enabled::active() {
                return Vec::new();
            }
            self.tail.extend_from_slice(bytes);
            let mut ids = Vec::new();
            for window in self.tail.windows(21) {
                if (window.starts_with(b"HL-O-") || window.starts_with(b"HL-I-"))
                    && window.ends_with(b"-END")
                {
                    if let Ok(text) = std::str::from_utf8(&window[5..17]) {
                        if let Ok(id) = u64::from_str_radix(text, 16) {
                            ids.push(id);
                        }
                    }
                }
            }
            let keep = self.tail.len().saturating_sub(20);
            self.tail.drain(..keep);
            ids.into_iter().next_back().into_iter().collect()
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            let _ = bytes;
            Vec::new()
        }
    }
}

pub(crate) fn surface_links(msg: &crate::protocol::ServerMessage, fingerprint: u64) {
    #[cfg(feature = "latency-prof")]
    {
        if !enabled::active() {
            return;
        }
        let (panes, revision) = match msg {
            crate::protocol::ServerMessage::PaneSurface(surface) => {
                (&surface.panes, surface.surface_revision)
            }
            crate::protocol::ServerMessage::PaneSurfacePatch(patch) => {
                (&patch.panes, patch.surface_revision)
            }
            _ => return,
        };
        record_at("surface.revision", revision, 0, fingerprint, now());
        for pane in panes {
            record_at(
                "surface.content",
                bytes_id(pane.pane_id.as_bytes()),
                pane.content_revision,
                fingerprint,
                now(),
            );
            record_at(
                "surface.geometry",
                bytes_id(pane.pane_id.as_bytes()),
                u64::from(pane.inner_rect.width) << 32 | u64::from(pane.inner_rect.height),
                fingerprint,
                now(),
            );
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (msg, fingerprint);
}
pub(crate) fn snapshot_links(snapshot: &crate::protocol::ClientShellSnapshot, framed: &[u8]) {
    #[cfg(feature = "latency-prof")]
    {
        if !enabled::active() {
            return;
        }
        let fingerprint = framed.get(4..).map(bytes_id).unwrap_or(0);
        for workspace in &snapshot.workspaces {
            record_at(
                "snapshot.label",
                bytes_id(workspace.label.as_bytes()),
                snapshot.revision,
                fingerprint,
                now(),
            );
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (snapshot, framed);
}

/// Raw input identity recognition is diagnostic-only; arbitrary input is unassigned.
#[derive(Default)]
pub(crate) struct InputStimuli {
    #[cfg(feature = "latency-prof")]
    tail: Vec<u8>,
}
impl InputStimuli {
    pub(crate) fn observe(&mut self, bytes: &[u8], stage: &'static str, scope: u64) {
        #[cfg(feature = "latency-prof")]
        {
            if !enabled::active() {
                return;
            }
            self.tail.extend_from_slice(bytes);
            for window in self.tail.windows(14) {
                if window[0] == b'!' && window[13] == b'~' {
                    if let Ok(text) = std::str::from_utf8(&window[1..13]) {
                        if let Ok(id) = u64::from_str_radix(text, 16) {
                            record_at(stage, id, 0, scope, now());
                        }
                    }
                }
            }
            let keep = self.tail.len().saturating_sub(13);
            self.tail.drain(..keep);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (bytes, stage, scope);
    }
    pub(crate) fn events(
        &mut self,
        events: &[crate::protocol::ClientPaneInputEvent],
        stage: &'static str,
        scope: u64,
    ) {
        #[cfg(feature = "latency-prof")]
        {
            if !enabled::active() {
                return;
            }
            for event in events {
                match event {
                    crate::protocol::ClientPaneInputEvent::Key {
                        code: crate::protocol::ClientKeyCode::Char(ch),
                        kind,
                        ..
                    } if *kind != crate::protocol::ClientKeyKind::Release => {
                        let mut encoded = [0u8; 4];
                        self.observe(ch.encode_utf8(&mut encoded).as_bytes(), stage, scope);
                    }
                    crate::protocol::ClientPaneInputEvent::TextCommit(text)
                    | crate::protocol::ClientPaneInputEvent::Paste(text) => {
                        self.observe(text.as_bytes(), stage, scope)
                    }
                    _ => {}
                }
            }
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (events, stage, scope);
    }
}

pub(crate) fn input_dispatch(
    events: &[crate::protocol::ClientPaneInputEvent],
    stage: &'static str,
    scope: u64,
) {
    #[cfg(feature = "latency-prof")]
    {
        thread_local! { static INPUTS: std::cell::RefCell<std::collections::HashMap<u64,InputStimuli>> = std::cell::RefCell::new(std::collections::HashMap::new()); }
        INPUTS.with(|inputs| {
            inputs
                .borrow_mut()
                .entry(scope)
                .or_default()
                .events(events, stage, scope)
        });
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (events, stage, scope);
}

/// Diagnostic identities stay outside every published wire codec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameIdentity {
    #[cfg(feature = "latency-prof")]
    pub(crate) fingerprint: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) connection: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) occurrence: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) offset: u64,
}
impl FrameIdentity {
    pub(crate) const fn empty() -> Self {
        Self {
            #[cfg(feature = "latency-prof")]
            fingerprint: 0,
            #[cfg(feature = "latency-prof")]
            connection: 0,
            #[cfg(feature = "latency-prof")]
            occurrence: 0,
            #[cfg(feature = "latency-prof")]
            offset: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QueuedFrame {
    pub(crate) identity: FrameIdentity,
    pub(crate) len: u64,
}

#[cfg(any(feature = "latency-prof", test))]
pub(crate) fn queued_frames(bytes: &[u8], connection: u64) -> Vec<QueuedFrame> {
    #[cfg(feature = "latency-prof")]
    {
        frame_ids(bytes)
            .into_iter()
            .map(|(fingerprint, len)| QueuedFrame {
                identity: FrameIdentity {
                    fingerprint,
                    connection,
                    occurrence: next_scope(),
                    offset: 0,
                },
                len,
            })
            .collect()
    }
    #[cfg(not(feature = "latency-prof"))]
    {
        let _ = (bytes, connection);
        Vec::new()
    }
}

pub(crate) fn record_frame_at(
    stage: &'static str,
    value: u64,
    scope: u64,
    ns: u64,
    frame: FrameIdentity,
) {
    #[cfg(feature = "latency-prof")]
    enabled::record_frame_at(stage, frame.fingerprint, value, scope, ns, frame);
    #[cfg(not(feature = "latency-prof"))]
    let _ = (stage, value, scope, ns, frame);
}

pub(crate) fn emit_queued_frames(stage: &'static str, frames: &[QueuedFrame], scope: u64, ns: u64) {
    for frame in frames {
        record_frame_at(stage, frame.len, scope, ns, frame.identity);
    }
}

/// Track framed-byte positions from immediately after the welcome. A lost record
/// cannot shift subsequent repeated payloads onto an earlier queue occurrence.
#[cfg(feature = "latency-prof")]
pub(crate) fn begin_connection_receive(stream: &crate::ipc::LocalStream) {
    if !enabled::active() {
        return;
    }
    let connection = next_scope();
    record(
        "client.connection",
        connection,
        u64::from(crate::platform::local_stream_peer_pid(stream).unwrap_or(0)),
    );
    READ_POSITION.with(|value| value.set((connection, 0)));
}

#[cfg(feature = "latency-prof")]
pub(crate) fn server_connection(stream: &crate::ipc::LocalStream, connection: u64, client_id: u64) {
    if !enabled::active() {
        return;
    }
    record_at(
        "server.connection",
        connection,
        u64::from(crate::platform::local_stream_peer_pid(stream).unwrap_or(0)),
        client_id,
        now(),
    );
}
