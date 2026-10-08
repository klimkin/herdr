#[path = "../../src/platform/latency.rs"]
mod clock;
pub use clock::monotonic_ns;

pub fn connect_local(
    path: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::prelude::*;
    #[cfg(unix)]
    let name = path.to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
    #[cfg(windows)]
    let name = path
        .to_string_lossy()
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?
        .into_owned();
    interprocess::local_socket::Stream::connect(name)
}

/// Nonblocking I/O shares one operation deadline, including fragmented replies.
pub struct DeadlineStream {
    stream: interprocess::local_socket::Stream,
    deadline: std::time::Instant,
}
impl DeadlineStream {
    pub fn new(
        stream: interprocess::local_socket::Stream,
        timeout: std::time::Duration,
    ) -> std::io::Result<Self> {
        use interprocess::local_socket::traits::Stream as _;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            deadline: std::time::Instant::now() + timeout,
        })
    }
    pub fn reset(&mut self, timeout: std::time::Duration) {
        self.deadline = std::time::Instant::now() + timeout;
    }
    pub fn expired(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }
    fn retry(&self) -> std::io::Result<()> {
        if self.expired() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local operation timed out",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        Ok(())
    }
}
impl std::io::Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.expired() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "local operation timed out",
                ));
            }
            match std::io::Read::read(&mut self.stream, bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.retry()?,
                result => return result,
            }
        }
    }
}
impl std::io::Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            if self.expired() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "local operation timed out",
                ));
            }
            match std::io::Write::write(&mut self.stream, bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.retry()?,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One outstanding PTY input write; blocked input cannot stall the stimulus loop.
pub struct InputWriter {
    sender: std::sync::mpsc::SyncSender<(Vec<u8>, std::sync::mpsc::Sender<std::io::Result<()>>)>,
}
impl InputWriter {
    pub fn new(mut writer: Box<dyn std::io::Write + Send>) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(
            Vec<u8>,
            std::sync::mpsc::Sender<std::io::Result<()>>,
        )>(1);
        std::thread::spawn(move || {
            for (bytes, done) in receiver {
                let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                let _ = done.send(result);
            }
        });
        Self { sender }
    }
}
impl std::io::Write for InputWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.sender
            .try_send((bytes.to_vec(), sender))
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "PTY input queue full or stopped",
                )
            })?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "PTY input completion unknown")
            })??;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
#[path = "platform/unix/process.rs"]
mod native;
#[cfg(windows)]
#[path = "platform/windows/process.rs"]
mod native;
pub use native::{kill_process_group, own_process_group};
