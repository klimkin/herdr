//! Controlled process used as a pane shell by the latency benchmark.
#[cfg(unix)]
#[path = "latency/platform/unix.rs"]
mod input;
#[cfg(windows)]
#[path = "latency/platform/windows.rs"]
mod input;
#[path = "../src/platform/latency.rs"]
mod platform;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

fn emit(output: &Mutex<std::io::Stdout>, marker: &str, row: u16) -> std::io::Result<()> {
    let mut output = output
        .lock()
        .map_err(|_| std::io::Error::other("output lock poisoned"))?;
    write!(output, "\x1b[{row};1H{marker}\x1b[K")?;
    output.flush()
}

fn events(event: serde_json::Value) -> std::io::Result<()> {
    use std::sync::OnceLock;
    static FILE: OnceLock<Mutex<std::io::Result<Option<std::fs::File>>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        Mutex::new(match std::env::var_os("HERDR_LATENCY_HELPER_EVENTS") {
            Some(path) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::path::PathBuf::from(path).join(format!("{}.jsonl", std::process::id())))
                .map(Some),
            None => Ok(None),
        })
    });
    let mut file = file
        .lock()
        .map_err(|_| std::io::Error::other("helper event lock poisoned"))?;
    match file.as_mut() {
        Ok(Some(file)) => writeln!(file, "{event}")?,
        Ok(None) => {}
        Err(error) => return Err(std::io::Error::new(error.kind(), error.to_string())),
    }
    Ok(())
}

fn control(mut stream: TcpStream, output: Arc<Mutex<std::io::Stdout>>) -> std::io::Result<()> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut load_thread = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let parts = line.split_whitespace().collect::<Vec<_>>();
        match parts.as_slice() {
            ["output", identity] => {
                let start = platform::monotonic_ns()?;
                emit(&output, &format!("HL-O-{identity}-END"), 1)?;
                let end = platform::monotonic_ns()?;
                writeln!(stream, "output {identity} {start} {end}")?;
            }
            ["load", frequency, burst] => {
                let frequency: u64 = frequency.parse().map_err(std::io::Error::other)?;
                let burst: u64 = burst.parse().map_err(std::io::Error::other)?;
                if frequency == 0 || frequency > 10000 || burst == 0 || burst > 1000 {
                    return Err(std::io::Error::other("invalid load"));
                }
                running.store(true, std::sync::atomic::Ordering::Relaxed);
                let active = running.clone();
                let output = output.clone();
                load_thread = Some(std::thread::spawn(move || {
                    let period = std::time::Duration::from_nanos(1_000_000_000 / frequency.max(1));
                    let mut next = std::time::Instant::now();
                    let mut sequence = 0u64;
                    let pane = std::env::var("HERDR_PANE_ID")
                        .unwrap_or_else(|_| std::process::id().to_string());
                    while active.load(std::sync::atomic::Ordering::Relaxed) {
                        for _ in 0..burst {
                            let raw = pane.rsplit('p').next().unwrap_or(&pane);
                            let marker = format!("LOAD-{raw}-{sequence:010}");
                            let start = platform::monotonic_ns().unwrap_or(0);
                            emit(&output, &marker, 2)?;
                            let end = platform::monotonic_ns().unwrap_or(0);
                            events(
                                serde_json::json!({"kind":"load","pid":std::process::id(),"pane":pane,"generation":sequence,"start_ns":start,"end_ns":end,"bytes":marker.len()+9}),
                            )?;
                            sequence += 1;
                        }
                        next += period;
                        // Bound catch-up after producer blockage.
                        if next < std::time::Instant::now() {
                            next = std::time::Instant::now();
                        }
                        std::thread::sleep(
                            next.saturating_duration_since(std::time::Instant::now()),
                        );
                    }
                    Ok::<(), std::io::Error>(())
                }));
                writeln!(stream, "load-ready")?;
            }
            ["stop"] => {
                running.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Some(worker) = load_thread.take() {
                    worker
                        .join()
                        .map_err(|_| std::io::Error::other("producer panicked"))??;
                }
                writeln!(stream, "stopped")?;
            }
            ["clock"] => writeln!(stream, "clock {}", platform::monotonic_ns()?)?,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid probe command",
                ))
            }
        }
        stream.flush()?;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    input::raw_stdin()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    if let Some(directory) = std::env::var_os("HERDR_LATENCY_HELPER_EVENTS") {
        let pane =
            std::env::var("HERDR_PANE_ID").unwrap_or_else(|_| std::process::id().to_string());
        std::fs::write(
            std::path::PathBuf::from(directory).join(format!("{pane}.port")),
            port.to_string(),
        )?;
    }
    let output = Arc::new(Mutex::new(std::io::stdout()));
    emit(&output, &format!("HL-READY-{port}-END"), 1)?;
    let writer = output.clone();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            if let Err(error) = control(stream, writer) {
                eprintln!("probe control: {error}");
            }
        }
    });
    let mut input = std::io::stdin().lock();
    let mut buffer = [0u8; 1024];
    let mut pending = Vec::new();
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'~') {
            let command = pending.drain(..=end).collect::<Vec<_>>();
            let Ok(command) = std::str::from_utf8(&command) else {
                continue;
            };
            let Some(identity) = command
                .strip_prefix('!')
                .and_then(|text| text.strip_suffix('~'))
            else {
                continue;
            };
            if identity.len() != 12 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let received = platform::monotonic_ns()?;
            let write_start = platform::monotonic_ns()?;
            emit(&output, &format!("HL-I-{identity}-END"), 1)?;
            let completed = platform::monotonic_ns()?;
            events(
                serde_json::json!({"kind":"echo","pid":std::process::id(),"identity":identity,"received_ns":received,"write_start_ns":write_start,"completed_ns":completed}),
            )?;
        }
        if pending.len() > 4096 {
            pending.clear();
        }
    }
    Ok(())
}
