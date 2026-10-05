//! Monotonic timestamps shared by local processes and benchmark terminal setup.

#[cfg(unix)]
#[path = "latency/unix.rs"]
mod native;
#[cfg(windows)]
#[path = "latency/windows.rs"]
mod native;

#[cfg(any(unix, windows))]
pub use native::monotonic_ns;

#[cfg(not(any(unix, windows)))]
pub fn monotonic_ns() -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "shared monotonic clock unavailable",
    ))
}
