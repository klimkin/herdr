//! Linux one-shot deadline waiting without millisecond timeout quantization.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;
use tokio::io::unix::AsyncFd;

pub(in crate::platform) struct NativeDeadlineWaiter {
    timer: AsyncFd<OwnedFd>,
    disarm_failed: bool,
}

impl NativeDeadlineWaiter {
    pub(in crate::platform) fn new() -> io::Result<Self> {
        // SAFETY: timerfd_create has no pointer arguments; flags request an
        // independently owned, nonblocking descriptor that cannot leak on exec.
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful timerfd_create returns a fresh descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            timer: AsyncFd::new(fd)?,
            disarm_failed: false,
        })
    }

    fn set(&self, value: libc::timespec) -> io::Result<()> {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: value,
        };
        // SAFETY: spec is initialized, descriptor remains owned, and a null
        // old-value pointer explicitly declines the previous timer settings.
        let result = unsafe {
            libc::timerfd_settime(
                self.timer.get_ref().as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn arm(&self, deadline: Instant) -> io::Result<()> {
        let now = Instant::now();
        let delay = deadline.saturating_duration_since(now);
        let mut target = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: target points to writable storage for one timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut target) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // Sample the kernel clock after Instant: sampling skew delays the
        // target slightly rather than causing presentation before eligibility.
        let seconds = delay.as_secs().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "deadline exceeds timer range")
        })?;
        target.tv_sec = target.tv_sec.checked_add(seconds).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "deadline exceeds timer range")
        })?;
        target.tv_nsec += delay.subsec_nanos() as libc::c_long;
        if target.tv_nsec >= 1_000_000_000 {
            target.tv_nsec -= 1_000_000_000;
            target.tv_sec = target.tv_sec.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "deadline exceeds timer range")
            })?;
        }
        self.set(target)
    }

    pub(in crate::platform) async fn wait(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        if self.disarm_failed {
            return Err(io::Error::other("failed to cancel native deadline timer"));
        }
        let Some(deadline) = deadline else {
            return std::future::pending().await;
        };
        if deadline <= Instant::now() {
            return Ok(());
        }
        self.arm(deadline)?;
        // The guard lives across await and disarms on select cancellation,
        // completion, or I/O error. A failed cancellation disables this backend
        // on the next wait, where the platform wrapper closes the descriptor.
        let armed = Armed(self);
        loop {
            let mut readiness = armed.0.timer.readable().await?;
            match readiness.try_io(|fd| {
                let mut expirations = 0_u64;
                // SAFETY: timerfd writes exactly one u64 expiration count into
                // writable storage; descriptor remains owned by AsyncFd.
                let count = unsafe {
                    libc::read(
                        fd.get_ref().as_raw_fd(),
                        (&mut expirations as *mut u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else if count as usize != std::mem::size_of::<u64>() {
                    Err(io::Error::other("short timerfd expiration read"))
                } else {
                    Ok(())
                }
            }) {
                Ok(Ok(())) if Instant::now() >= deadline => return Ok(()),
                // Protect eligibility even if readiness came from an older arm.
                Ok(Ok(())) => armed.0.arm(deadline)?,
                // try_io clears cached readiness on EAGAIN before waiting again.
                Err(_) => {}
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Err(error),
            }
        }
    }
}

struct Armed<'a>(&'a mut NativeDeadlineWaiter);

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.set(libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }) {
            self.0.disarm_failed = true;
            tracing::warn!(%error, "failed to disarm native deadline timer");
        }
    }
}
