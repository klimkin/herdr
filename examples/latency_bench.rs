//! End-to-end probes through owned PTYs. APIs only prepare/inspect the session.
#[path = "latency/observer.rs"]
mod observer;
#[path = "latency/platform.rs"]
mod platform;
#[path = "latency/report.rs"]
mod report;
#[path = "latency/transport.rs"]
mod transport;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

struct Client {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    input: platform::InputWriter,
    screen: Arc<Mutex<observer::Observer>>,
    completed: mpsc::Receiver<observer::Observation>,
    delay_ms: Arc<AtomicU64>,
    observer_ns: Arc<AtomicU64>,
    observer_bytes: Arc<AtomicU64>,
}

struct Session {
    base: PathBuf,
    output: PathBuf,
    tracy_port: u16,
    binary: PathBuf,
    probe: PathBuf,
    name: String,
    config: PathBuf,
    clients: Vec<Client>,
    server: Option<std::process::Child>,
}

impl Session {
    fn configure(&self, command: &mut CommandBuilder) {
        for name in [
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_SESSION",
            "HERDR_WORKSPACE_ID",
            "HERDR_TAB_ID",
            "HERDR_PANE_ID",
            "HERDR_BIN_PATH",
            "HERDR_STARTUP_CWD",
        ] {
            command.env_remove(name);
        }
        command.env("XDG_CONFIG_HOME", self.base.join("config"));
        command.env("XDG_STATE_HOME", self.base.join("state"));
        command.env("XDG_DATA_HOME", self.base.join("data"));
        command.env("XDG_CACHE_HOME", self.base.join("cache"));
        command.env("XDG_RUNTIME_DIR", self.base.join("runtime"));
        command.env("HERDR_CONFIG_PATH", &self.config);
        command.env("HERDR_LATENCY_HELPER_EVENTS", self.output.join("helpers"));
        if std::env::var_os("HERDR_LATENCY_TRACE_DIR").is_some() {
            command.env("HERDR_LATENCY_TRACE_DIR", self.output.join("traces"));
        }
        command.env("HERDR_DISABLE_SOUND", "1");
        command.env("TERM", "xterm-256color");
        command.env("SHELL", &self.probe);
        command.cwd(&self.base);
    }

    fn start_server(&mut self) -> Result<()> {
        let mut command = std::process::Command::new(&self.binary);
        command.args(["server", "--session", &self.name]);
        for name in [
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_SESSION",
            "HERDR_WORKSPACE_ID",
            "HERDR_TAB_ID",
            "HERDR_PANE_ID",
            "HERDR_BIN_PATH",
            "HERDR_STARTUP_CWD",
        ] {
            command.env_remove(name);
        }
        for (name, directory) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            command.env(name, self.base.join(directory));
        }
        command
            .env("HERDR_CONFIG_PATH", &self.config)
            .env("HERDR_LATENCY_HELPER_EVENTS", self.output.join("helpers"))
            .env("HERDR_DISABLE_SOUND", "1")
            .env("TERM", "xterm-256color")
            .env("SHELL", &self.probe)
            .current_dir(&self.base)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(self.output.join("server.stdout"))?)
            .stderr(std::fs::File::create(self.output.join("server.stderr"))?);
        if std::env::var_os("HERDR_LATENCY_TRACE_DIR").is_some() {
            command.env("HERDR_LATENCY_TRACE_DIR", self.output.join("traces"));
        }
        command.env("TRACY_PORT", self.tracy_port.to_string());
        platform::own_process_group(&mut command);
        self.server = Some(command.spawn()?);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.api("pane.list", json!({})).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("owned server startup timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn api(&self, method: &str, params: Value) -> Result<Value> {
        let directory = if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        };
        let socket = self
            .base
            .join("config")
            .join(directory)
            .join("sessions")
            .join(&self.name)
            .join("herdr.sock");
        let mut stream = platform::DeadlineStream::new(
            platform::connect_local(&socket)?,
            Duration::from_secs(2),
        )?;
        writeln!(
            stream,
            "{}",
            json!({"id":"benchmark", "method":method,"params":params})
        )?;
        stream.flush()?;
        let mut line = String::new();
        std::io::BufReader::new(stream).read_line(&mut line)?;
        let response: Value = serde_json::from_str(&line)?;
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            return Err(format!("{method}: {error}").into());
        }
        Ok(response["result"].clone())
    }

    fn attach(&mut self, index: usize) -> Result<()> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(&self.binary);
        command.args(["--session", &self.name]);
        self.configure(&mut command);
        command.env(
            "TRACY_PORT",
            (self.tracy_port + 1 + index as u16).to_string(),
        );
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let input = platform::InputWriter::new(pair.master.take_writer()?);
        let screen = Arc::new(Mutex::new(observer::Observer::new(120, 40)?));
        let screen_thread = screen.clone();
        let (sender, completed) = mpsc::channel();
        let delay_ms = Arc::new(AtomicU64::new(0));
        let reader_delay = delay_ms.clone();
        let observer_ns = Arc::new(AtomicU64::new(0));
        let observer_bytes = Arc::new(AtomicU64::new(0));
        let parse_cost = observer_ns.clone();
        let byte_count = observer_bytes.clone();
        let mut raw = std::fs::File::create(self.output.join(format!("client-{index}.vt")))?;
        std::thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            loop {
                std::thread::sleep(Duration::from_millis(reader_delay.load(Ordering::Relaxed)));
                let count = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let Ok(received) = platform::monotonic_ns() else {
                    break;
                };
                let Ok(mut screen) = screen_thread.lock() else {
                    break;
                };
                let parse_start = Instant::now();
                let observations = match screen.feed(&buffer[..count], received) {
                    Ok(values) => values,
                    Err(_) => break,
                };
                parse_cost.fetch_add(parse_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                byte_count.fetch_add(count as u64, Ordering::Relaxed);
                drop(screen);
                for observation in observations {
                    let _ = sender.send(observation);
                }
                if raw.write_all(&buffer[..count]).is_err() {
                    break;
                }
            }
        });
        self.clients.push(Client {
            child,
            _master: pair.master,
            input,
            screen,
            completed,
            delay_ms,
            observer_ns,
            observer_bytes,
        });
        Ok(())
    }

    fn wait_text(&self, index: usize, pattern: &str) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = self.clients[index]
                .screen
                .lock()
                .map_err(|_| "screen poisoned")?
                .text()?;
            if text.contains(pattern) {
                return Ok(text);
            }
            if Instant::now() >= deadline {
                return Err(format!("client {index} missing {pattern}: {text}").into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn expect_all(&mut self, marker: &str, ns: u64) -> Result<()> {
        for client in &self.clients {
            client
                .screen
                .lock()
                .map_err(|_| "screen poisoned")?
                .expect(marker.to_owned(), ns);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.api("server.stop", json!({}));
        for client in &mut self.clients {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if client.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Graceful server shutdown gives clients time to drain diagnostics.
            if client.child.try_wait().ok().flatten().is_none() {
                let _ = client.child.kill();
            }
        }
        if let Some(mut server) = self.server.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut exited = false;
            while Instant::now() < deadline {
                if server.try_wait().ok().flatten().is_some() {
                    exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if !exited {
                platform::kill_process_group(&mut server);
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if server.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[derive(serde::Serialize)]
struct Sample {
    path: String,
    identity: String,
    intended_ns: u64,
    injected_ns: u64,
    process_start_ns: Option<u64>,
    process_end_ns: Option<u64>,
    helper_received_ns: Option<u64>,
    observed_ns: Vec<Option<u64>>,
    outcome: String,
}

fn argument(name: &str, fallback: &str) -> String {
    let args = std::env::args().collect::<Vec<_>>();
    args.windows(2)
        .find(|args| args[0] == name)
        .map_or_else(|| fallback.to_owned(), |args| args[1].clone())
}

fn connect_probe(text: &str) -> Result<TcpStream> {
    let port = text
        .split("HL-READY-")
        .nth(1)
        .and_then(|text| text.split("-END").next())
        .ok_or("probe port absent")?
        .trim();
    let stream = TcpStream::connect(format!("127.0.0.1:{port}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok(stream)
}

fn main() -> Result<()> {
    let binary = PathBuf::from(argument("--binary", "target/debug/herdr")).canonicalize()?;
    let probe =
        PathBuf::from(argument("--probe", "target/debug/examples/latency_probe")).canonicalize()?;
    let tracy_port: u16 = argument("--tracy-port", "8086").parse()?;
    if tracy_port > 65000 {
        return Err("invalid Tracy port".into());
    }
    let run_id = format!("{}-{}", std::process::id(), platform::monotonic_ns()?);
    let output = PathBuf::from(argument("--output", ".local/latency-run")).join(&run_id);
    std::fs::create_dir_all(output.join("helpers"))?;
    let output = output.canonicalize()?;
    let warmups: usize = argument("--warmups", "10").parse()?;
    let burst: u64 = argument("--burst", "1").parse()?;
    let max_pending: usize = argument("--max-pending", "64").parse()?;
    let count: usize = argument("--samples", "40").parse()?;
    let clients: usize = argument("--clients", "1").parse()?;
    let panes: usize = argument("--panes", "1").parse()?;
    let stall_ms: u64 = argument("--stall-ms", "0").parse()?;
    if stall_ms > 10000 {
        return Err("stall exceeds 10-second bound".into());
    }
    let slow_reader_ms: u64 = argument("--slow-reader-ms", "0").parse()?;
    let interval_ms: u64 = argument("--interval-ms", "30").parse()?;
    let load_hz: u64 = argument("--load-hz", "60").parse()?;
    let path = argument("--path", "output");
    let load = argument("--load", "quiet");
    let layout = argument("--layout", "tabs");
    if burst == 0
        || burst > 1000
        || max_pending == 0
        || max_pending > 4096
        || warmups > 1000
        || interval_ms > 10000
        || count == 0
        || count > 100_000
        || clients == 0
        || panes == 0
        || panes > 64
        || clients > 16
        || load_hz == 0
        || load_hz > 10_000
        || !matches!(layout.as_str(), "tabs" | "active")
        || (layout == "active" && load == "hidden")
        || !matches!(path.as_str(), "output" | "echo" | "action")
        || !matches!(load.as_str(), "quiet" | "visible" | "hidden")
    {
        return Err("invalid sample/client/pane count, path, or load".into());
    }
    let base = std::env::temp_dir().join(format!("hl-{run_id}"));
    std::fs::create_dir(&base)?;
    let config = base.join("config.toml");
    let shell = serde_json::to_string(&probe.to_string_lossy())?;
    std::fs::write(&config, format!("onboarding = false\n[experimental]\nallow_nested = true\n[terminal]\ndefault_shell = {shell}\nshell_mode = \"non_login\"\n[ui]\nwindow_title = \"\"\n[update]\nversion_check = false\nmanifest_check = false\n"))?;
    let mut session = Session {
        base,
        output: output.clone(),
        tracy_port,
        binary,
        probe,
        name: "latency".into(),
        config,
        clients: Vec::new(),
        server: None,
    };
    session.start_server()?;
    session.attach(0)?;
    let text = session.wait_text(0, "HL-READY-")?;
    let mut control = connect_probe(&text)?;
    let mut control_reader = std::io::BufReader::new(control.try_clone()?);
    let before = platform::monotonic_ns()?;
    writeln!(control, "clock")?;
    let mut clock = String::new();
    control_reader.read_line(&mut clock)?;
    let after = platform::monotonic_ns()?;
    let helper: u64 = clock
        .split_whitespace()
        .nth(1)
        .ok_or("clock response absent")?
        .parse()?;
    if helper < before || helper > after {
        return Err("helper clock does not share host domain".into());
    }
    let pane_list = session.api("pane.list", json!({}))?;
    let workspace = pane_list["panes"][0]["workspace_id"]
        .as_str()
        .ok_or("workspace absent")?
        .to_owned();
    let root = pane_list["panes"][0]["pane_id"]
        .as_str()
        .ok_or("root pane absent")?
        .to_owned();
    let mut pane_ids = vec![root.clone()];
    // Balance a three-column grid so 15 panes retain readable probe identities.
    let mut areas = vec![(root, 96u16, 36u16)];
    let mut background = Vec::new();
    for _ in 1..panes {
        let active = layout == "active";
        let created = if active {
            let index = areas
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, w, h))| u32::from(*w) * u32::from(*h))
                .map(|(i, _)| i)
                .ok_or("pane area absent")?;
            let (target, width, height) = areas[index].clone();
            let horizontal = width > 36;
            let direction = if horizontal { "right" } else { "down" };
            let ratio = if width > 70 { 1.0 / 3.0 } else { 0.5 };
            let created = session.api("pane.split", json!({"workspace_id":workspace,"target_pane_id":target,"direction":direction,"ratio":ratio,"focus":false}))?;
            let pane = created["pane"]["pane_id"]
                .as_str()
                .ok_or("split pane absent")?
                .to_owned();
            let first = if horizontal {
                (width as f32 * ratio) as u16
            } else {
                height / 2
            };
            areas[index] = if horizontal {
                (target, first, height)
            } else {
                (target, width, first)
            };
            areas.push(if horizontal {
                (pane, width - first, height)
            } else {
                (pane, width, height - first)
            });
            created
        } else {
            session.api(
                "tab.create",
                json!({"workspace_id":workspace,"focus":false}),
            )?
        };
        let pane = created["root_pane"]["pane_id"]
            .as_str()
            .or_else(|| created["pane"]["pane_id"].as_str())
            .ok_or("created pane absent")?;
        pane_ids.push(pane.to_owned());
        if load != "quiet" {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(port) =
                    std::fs::read_to_string(output.join("helpers").join(format!("{pane}.port")))
                {
                    if let Ok(stream) = connect_probe(&format!("HL-READY-{}-END", port.trim())) {
                        background.push(stream);
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    return Err("background probe not ready".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let pane_layout = session.api("pane.layout", json!({"pane_id":pane_ids[0]}))?;
    for index in 1..clients {
        session.attach(index)?;
        session.wait_text(index, "HL-READY-")?;
    }
    if stall_ms > 0 && clients < 2 {
        return Err("stall requires multiple clients".into());
    }
    if slow_reader_ms > 0 {
        if clients < 2 {
            return Err("slow reader requires at least two clients".into());
        }
        session.clients[clients - 1]
            .delay_ms
            .store(slow_reader_ms, Ordering::Relaxed);
    }
    if load == "visible" {
        writeln!(control, "load {load_hz} {burst}")?;
        let mut line = String::new();
        control_reader.read_line(&mut line)?;
    }
    for stream in &mut background {
        writeln!(stream, "load {load_hz} {burst}")?;
        let mut ack = String::new();
        std::io::BufReader::new(stream.try_clone()?).read_line(&mut ack)?;
        if ack.trim() != "load-ready" {
            return Err("producer start ACK absent".into());
        }
    }
    control_reader.get_ref().set_read_timeout(None)?;
    let (write_sender, write_times) = mpsc::channel();
    let (stop_sender, stopped) = mpsc::channel();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        match control_reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim() == "stopped" {
            let _ = stop_sender.send(());
        }
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() == 4 && parts[0] == "output" {
            if let (Ok(start), Ok(end)) = (parts[2].parse::<u64>(), parts[3].parse::<u64>()) {
                let _ = write_sender.send((parts[1].to_owned(), start, end));
            }
        }
    });
    let mut samples = Vec::<Sample>::new();
    let mut positions = BTreeMap::new();
    let mut seed = 92341u64;
    let mut intended = platform::monotonic_ns()?;
    let started_ns = platform::monotonic_ns()?;
    if stall_ms > 0 {
        let delay = session.clients[clients - 1].delay_ms.clone();
        delay.store(stall_ms, Ordering::Relaxed);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(stall_ms));
            delay.store(0, Ordering::Relaxed);
        });
    }
    for sequence in 0..count + warmups {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        intended += interval_ms * 1_000_000 + seed % (interval_ms.max(1) * 1_000_000);
        std::thread::sleep(Duration::from_nanos(
            intended.saturating_sub(platform::monotonic_ns()?),
        ));
        let identity = format!("{sequence:012x}");
        let marker = match path.as_str() {
            "output" => format!("HL-O-{identity}-END"),
            "echo" => format!("HL-I-{identity}-END"),
            _ => format!("A{sequence:06}"),
        };
        if path == "action" {
            session.clients[0].input.write_all(b"\x02W")?;
            session.wait_text(0, "rename workspace")?;
            session.clients[0].input.write_all(b"\x03")?;
            session.clients[0].input.write_all(marker.as_bytes())?;
            session.wait_text(0, &marker)?;
        }
        let injected = platform::monotonic_ns()?;
        session.expect_all(&marker, injected)?;
        let sample = Sample {
            path: path.clone(),
            identity,
            intended_ns: intended,
            injected_ns: injected,
            process_start_ns: None,
            process_end_ns: None,
            helper_received_ns: None,
            observed_ns: vec![None; clients],
            outcome: "pending".into(),
        };
        let outstanding = samples
            .iter()
            .filter(|sample| {
                sample.observed_ns.iter().any(Option::is_none) && sample.outcome == "pending"
            })
            .count();
        let injection = if outstanding >= max_pending {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "pending probe limit",
            ))
        } else {
            match path.as_str() {
                "output" => writeln!(control, "output {}", sample.identity),
                "echo" => session.clients[0]
                    .input
                    .write_all(format!("!{}~", sample.identity).as_bytes()),
                _ => session.clients[0].input.write_all(b"\r"),
            }
        };
        let mut sample = sample;
        if let Err(error) = injection {
            for client in &session.clients {
                if let Ok(mut screen) = client.screen.lock() {
                    screen.cancel(&marker);
                }
            }
            sample.outcome = if error.kind() == std::io::ErrorKind::TimedOut {
                format!("interrupted: {error}")
            } else {
                format!("rejected: {error}")
            };
        }
        positions.insert(marker, samples.len());
        samples.push(sample);
        for (index, client) in session.clients.iter().enumerate() {
            for observation in client.completed.try_iter() {
                if let Some(position) = positions.get(&observation.marker) {
                    samples[*position].observed_ns[index] = Some(observation.received_ns);
                }
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for (index, client) in session.clients.iter().enumerate() {
            for observed in client.completed.try_iter() {
                if let Some(position) = positions.get(&observed.marker) {
                    samples[*position].observed_ns[index] = Some(observed.received_ns);
                }
            }
        }
        if samples
            .iter()
            .all(|sample| sample.observed_ns.iter().all(Option::is_some))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for sample in &mut samples {
        if sample.outcome != "pending" {
            continue;
        }
        sample.outcome = if sample.observed_ns.iter().all(Option::is_some) {
            "presented"
        } else {
            "timeout"
        }
        .into();
    }
    // Stop all producers before taking event snapshots; ACK follows the last write.
    if load == "visible" {
        writeln!(control, "stop")?;
        stopped
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "foreground producer stop ACK absent")?;
    }
    for stream in &mut background {
        writeln!(stream, "stop")?;
        let mut ack = String::new();
        std::io::BufReader::new(stream.try_clone()?).read_line(&mut ack)?;
        if ack.trim() != "stopped" {
            return Err("producer stop ACK absent".into());
        }
    }
    // Final generations remain pending when their matching output never arrives.
    std::thread::sleep(Duration::from_millis(100));
    let helper_deadline = Instant::now() + Duration::from_secs(2);
    let mut writes = Vec::new();
    while Instant::now() < helper_deadline {
        match write_times.recv_timeout(Duration::from_millis(20)) {
            Ok(value) => writes.push(value),
            Err(_) => {
                if path != "output" || writes.len() >= count + warmups {
                    break;
                }
            }
        }
    }
    for (identity, start, end) in writes {
        if let Some(sample) = samples
            .iter_mut()
            .find(|sample| sample.identity == identity)
        {
            sample.process_start_ns = Some(start);
            sample.process_end_ns = Some(end);
        }
    }
    let mut load_events = Vec::<Value>::new();
    for entry in std::fs::read_dir(output.join("helpers"))? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            for line in std::fs::read_to_string(entry.path())?.lines() {
                let event: Value = serde_json::from_str(line)?;
                if event["kind"] == "load" {
                    load_events.push(event.clone());
                }
                if let Some(sample) = samples
                    .iter_mut()
                    .find(|sample| event["identity"] == sample.identity)
                {
                    sample.helper_received_ns = event["received_ns"].as_u64();
                    sample.process_start_ns =
                        event["write_start_ns"].as_u64().or(sample.process_start_ns);
                    sample.process_end_ns = event["completed_ns"].as_u64();
                }
            }
        }
    }
    let finished_ns = platform::monotonic_ns()?;
    samples.drain(..warmups);
    let summaries = (0..clients)
        .map(|client| {
            let values = samples
                .iter()
                .filter_map(|sample| {
                    sample.observed_ns[client].and_then(|end| {
                        if path == "output" {
                            end.checked_sub(sample.process_start_ns?)
                        } else {
                            end.checked_sub(sample.injected_ns)
                        }
                    })
                })
                .collect::<Vec<_>>();
            report::summarize(&values, count, 20_000_000)
        })
        .collect::<Vec<_>>();
    let directory = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let socket = session
        .base
        .join("config")
        .join(directory)
        .join("sessions")
        .join(&session.name)
        .join("herdr-client.sock");
    let transport_result=transport::sample(&socket,20)
        .map(|values|json!({"summary":report::summarize(&values,20,20_000_000),"values_ns":values,"endpoint":"matched socket RTT, excludes application presentation"}))
        .unwrap_or_else(|error|json!({"unavailable":error.to_string()}));
    let fanout = samples
        .iter()
        .filter_map(|sample| {
            let times = sample
                .observed_ns
                .iter()
                .copied()
                .collect::<Option<Vec<_>>>()?;
            Some(times.iter().max()?.saturating_sub(*times.iter().min()?))
        })
        .collect::<Vec<_>>();
    let freshness = session
        .clients
        .iter()
        .map(|client| {
            client
                .screen
                .lock()
                .map(|screen| screen.newest_load_generation)
                .unwrap_or(None)
        })
        .collect::<Vec<_>>();
    let presented_load = session
        .clients
        .iter()
        .map(|client| {
            client
                .screen
                .lock()
                .map(|screen| screen.load_observations.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let observer_cost = session.clients.iter().map(|client| json!({"parse_ns":client.observer_ns.load(Ordering::Relaxed),"bytes":client.observer_bytes.load(Ordering::Relaxed)})).collect::<Vec<_>>();
    let binary_sha256 = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(std::fs::read(&session.binary)?))
    };
    let owned_server_pid = session.server.as_ref().map(std::process::Child::id);
    let first_intended_ns = samples.first().map(|sample| sample.intended_ns);
    let last_intended_ns = samples.last().map(|sample| sample.intended_ns);
    let echo_legs = if path == "echo" {
        let legs = samples.iter().map(|sample| json!({"identity":sample.identity,"forward_ns":sample.helper_received_ns.and_then(|ns|ns.checked_sub(sample.injected_ns)),"helper_ns":sample.process_end_ns.zip(sample.helper_received_ns).and_then(|(end,start)|end.checked_sub(start)),"return_ns":sample.observed_ns.iter().map(|ns|ns.and_then(|ns|ns.checked_sub(sample.process_start_ns?))).collect::<Vec<_>>()})).collect::<Vec<_>>();
        json!(legs)
    } else {
        Value::Null
    };
    let mut report = json!({"run_id":run_id,"path":path,"load":load,"load_hz":load_hz,"panes":panes,"clients":clients,"samples":samples,"summary":summaries});
    let metadata = json!({"started_ns":started_ns,"finished_ns":finished_ns,"duration_ns":finished_ns-started_ns,"warmups":warmups,"burst":burst,"max_pending":max_pending,"interval_ms":interval_ms,"seed":92341,"layout":layout,"pane_ids":pane_ids,"estimated_pane_areas":areas,"pane_layout":pane_layout,"slow_reader_ms":slow_reader_ms,"stall_ms":stall_ms});
    let measurement = json!({"echo_legs":echo_legs,"observer_cost":observer_cost,"binary_sha256":binary_sha256,"server_pid":owned_server_pid,"client_pids":session.clients.iter().map(|client|client.child.process_id()).collect::<Vec<_>>(),"base":session.base,"fanout_spread":report::summarize(&fanout,count,20_000_000),"newest_presented_load_generation":freshness,"load_events":load_events,"presented_load":presented_load,"transport":transport_result});
    let environment = json!({"offered_samples":count,"measurement_first_intended_ns":first_intended_ns,"measurement_last_intended_ns":last_intended_ns,"percentile_method":"nearest rank; p99.9 requires 10000 completions","clock":"host monotonic; observer includes scheduling and reconstruction","platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"profiler":"tracy-client 0.19.0; Tracy 0.14.1","scheduler_evidence":"unavailable in baseline mode","clock_validation_ns":[before,helper,after],"geometry":[120,40],"endpoint":"outer-PTY committed terminal bytes","budget_ns":20_000_000,"budget_kind":"diagnostic, not production SLO","binary":session.binary,"probe":session.probe});
    for fields in [metadata, measurement, environment] {
        if let (Some(destination), Some(fields)) = (report.as_object_mut(), fields.as_object()) {
            destination.extend(fields.clone());
        }
    }
    std::fs::write(
        output.join("samples.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report["summary"])?);
    println!("Report: {}", output.join("samples.json").display());
    Ok(())
}
