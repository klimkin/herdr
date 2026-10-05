//! Stable endpoint health RTT control, independent of application presentation.
use serde_json::json;
use std::io::{Read, Write};

fn control(kind: &str, data: &str) -> std::io::Result<Vec<u8>> {
    // Generation-1 EndpointControl has immutable wire tag 20.
    let payload = bincode::serde::encode_to_vec((20u32, kind, data), bincode::config::standard())
        .map_err(std::io::Error::other)?;
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend(payload);
    Ok(frame)
}

fn read(stream: &mut super::platform::DeadlineStream) -> std::io::Result<Option<(String, String)>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix)?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > 32 * 1024 * 1024 {
        return Err(std::io::Error::other("oversized transport frame"));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload)?;
    let (tag, _): (u32, usize) =
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(std::io::Error::other)?;
    if tag != 20 {
        return Ok(None);
    }
    let ((_, kind, data), consumed): ((u32, String, String), usize) =
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(std::io::Error::other)?;
    if consumed != payload.len() {
        return Err(std::io::Error::other("trailing endpoint bytes"));
    }
    Ok(Some((kind, data)))
}

pub fn sample(socket: &std::path::Path, count: usize) -> std::io::Result<Vec<u64>> {
    let local = super::platform::connect_local(socket)?;
    let mut stream =
        super::platform::DeadlineStream::new(local, std::time::Duration::from_secs(3))?;
    let hello = json!({"generation":1,"cell_width_px":8,"cell_height_px":16,"pixel_mouse":false,"direct_graphics":false,"endpoint_keybindings":false,"mouse_capture":false,"surface_size":{"cols":120,"rows":40},"surface_active":false,"snapshot_codecs":["shell.snapshot.v1"],"surface_codecs":["shell.surface.v1"],"input_codecs":["shell.input.semantic.v1"],"blob_codecs":["shell.blob.v1"]});
    stream.write_all(&control("endpoint.hello.v1", &hello.to_string())?)?;
    loop {
        if let Some((kind, data)) = read(&mut stream)? {
            if kind == "endpoint.welcome.v1" {
                let welcome: serde_json::Value =
                    serde_json::from_str(&data).map_err(std::io::Error::other)?;
                if welcome.get("error").is_some_and(|error| !error.is_null())
                    || !welcome["capabilities"]
                        .as_array()
                        .is_some_and(|values| values.iter().any(|value| value == "health_check"))
                {
                    return Err(std::io::Error::other(format!(
                        "endpoint health unavailable: {welcome}"
                    )));
                }
            }
            if kind == "shell.snapshot.v1" {
                break;
            }
        }
    }
    let mut samples = Vec::new();
    for index in 0..count + 10 {
        let identity = format!("latency-{index}");
        let frame = control("endpoint.health.ping.v1", &identity)?;
        stream.reset(std::time::Duration::from_secs(3));
        let start = super::platform::monotonic_ns()?;
        stream.write_all(&frame)?;
        stream.flush()?;
        loop {
            if let Some((kind, data)) = read(&mut stream)? {
                if kind == "endpoint.health.pong.v1" && data == identity {
                    if index >= 10 {
                        samples.push(super::platform::monotonic_ns()?.saturating_sub(start));
                    }
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(samples)
}
