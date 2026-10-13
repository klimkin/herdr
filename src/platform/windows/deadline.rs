//! Cancellation-safe Windows waitable-timer deadlines without idle polling.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CancelWaitableTimer, CreateEventW, CreateWaitableTimerExW, ResetEvent, SetEvent,
    SetWaitableTimer, WaitForMultipleObjects, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    TIMER_ALL_ACCESS,
};

pub(in crate::platform) struct NativeDeadlineWaiter {
    commands: mpsc::Sender<Command>,
    wake: Arc<OwnedHandle>,
    worker: Option<JoinHandle<()>>,
    worker_done: Option<mpsc::Receiver<()>>,
    next_id: u64,
    failed: Arc<AtomicBool>,
}

enum Command {
    Arm {
        id: u64,
        deadline: Instant,
        completed: oneshot::Sender<io::Result<()>>,
    },
    Cancel {
        id: u64,
    },
    Shutdown,
}

struct ActiveWait {
    id: u64,
    deadline: Instant,
    completed: oneshot::Sender<io::Result<()>>,
}

impl NativeDeadlineWaiter {
    pub(in crate::platform) fn new() -> io::Result<Self> {
        let timer = create_timer()?;
        let wake = create_wake_event()?;
        let wake = Arc::new(wake);
        let worker_wake = Arc::clone(&wake);
        let failed = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        let (commands, receiver) = mpsc::channel();
        let (worker_done_tx, worker_done) = mpsc::channel();
        let worker = crate::thread_spawn::spawn_named("herdr-deadline", move || {
            run_worker(timer, worker_wake, receiver, worker_failed);
            let _ = worker_done_tx.send(());
        })?;
        Ok(Self {
            commands,
            wake,
            worker: Some(worker),
            worker_done: Some(worker_done),
            next_id: 1,
            failed,
        })
    }

    pub(in crate::platform) async fn wait(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("native deadline timer worker failed"));
        }
        let Some(deadline) = deadline else {
            return std::future::pending().await;
        };
        if deadline <= Instant::now() {
            return Ok(());
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("native deadline identity exhausted"))?;
        let (completed, receiver) = oneshot::channel();
        self.send(Command::Arm {
            id,
            deadline,
            completed,
        })?;
        let mut armed = Armed {
            commands: &self.commands,
            wake: &self.wake,
            failed: &self.failed,
            id,
            active: true,
        };
        let result = receiver.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native deadline timer worker stopped",
            )
        })?;
        armed.active = false;
        result
    }

    fn send(&self, command: Command) -> io::Result<()> {
        self.commands.send(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native deadline timer worker stopped",
            )
        })?;
        signal(&self.wake)
    }
}

impl Drop for NativeDeadlineWaiter {
    fn drop(&mut self) {
        let sent = self.commands.send(Command::Shutdown).is_ok();
        if !sent {
            tracing::warn!("native deadline timer worker stopped before shutdown");
        }
        if let Err(error) = signal(&self.wake) {
            tracing::warn!(%error, "failed to wake native deadline timer for shutdown");
        }
        let stopped = self.worker_done.take().is_some_and(|done| {
            matches!(
                done.recv_timeout(Duration::from_secs(1)),
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected)
            )
        });
        if stopped {
            if let Some(worker) = self.worker.take() {
                if worker.join().is_err() {
                    tracing::warn!("native deadline timer worker panicked");
                }
            }
        } else {
            tracing::warn!("native deadline timer worker did not stop within one second");
        }
    }
}

struct Armed<'a> {
    commands: &'a mpsc::Sender<Command>,
    wake: &'a Arc<OwnedHandle>,
    failed: &'a Arc<AtomicBool>,
    id: u64,
    active: bool,
}

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.commands.send(Command::Cancel { id: self.id }).is_err() {
            tracing::warn!("native deadline timer worker stopped before cancellation");
            self.failed.store(true, Ordering::Release);
            return;
        }
        if let Err(error) = signal(self.wake) {
            tracing::warn!(%error, "failed to wake native deadline timer for cancellation");
            self.failed.store(true, Ordering::Release);
        }
    }
}

fn create_timer() -> io::Result<OwnedHandle> {
    let mut timer = unsafe {
        CreateWaitableTimerExW(
            std::ptr::null(),
            std::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer.is_null() {
        tracing::debug!(
            error = %io::Error::last_os_error(),
            "high-resolution waitable timer unavailable; using standard waitable timer"
        );
        timer = unsafe {
            CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS)
        };
    }
    if timer.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(timer.cast()) })
}

fn create_wake_event() -> io::Result<OwnedHandle> {
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(event.cast()) })
}

fn run_worker(
    timer: OwnedHandle,
    wake: Arc<OwnedHandle>,
    receiver: mpsc::Receiver<Command>,
    failed: Arc<AtomicBool>,
) {
    let handles = [raw_handle(&wake), raw_handle(&timer)];
    let mut active: Option<ActiveWait> = None;
    loop {
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
        match result {
            WAIT_OBJECT_0 => {
                if unsafe { ResetEvent(raw_handle(&wake)) } == 0 {
                    fail_worker(
                        &mut active,
                        &failed,
                        io::Error::last_os_error(),
                        "failed to reset native deadline wake event",
                    );
                    return;
                }
                loop {
                    match receiver.try_recv() {
                        Ok(Command::Arm {
                            id,
                            deadline,
                            completed,
                        }) => {
                            if let Some(replaced) = active.take() {
                                let _ = replaced.completed.send(Err(io::Error::new(
                                    io::ErrorKind::Interrupted,
                                    "native deadline wait replaced",
                                )));
                            }
                            let wait = ActiveWait {
                                id,
                                deadline,
                                completed,
                            };
                            match arm_timer(&timer, deadline) {
                                Ok(()) => active = Some(wait),
                                Err(error) => {
                                    active = Some(wait);
                                    fail_worker(
                                        &mut active,
                                        &failed,
                                        error,
                                        "failed to arm native deadline timer",
                                    );
                                    return;
                                }
                            }
                        }
                        Ok(Command::Cancel { id }) => {
                            if active.as_ref().is_some_and(|wait| wait.id == id) {
                                if unsafe { CancelWaitableTimer(raw_handle(&timer)) } == 0 {
                                    fail_worker(
                                        &mut active,
                                        &failed,
                                        io::Error::last_os_error(),
                                        "failed to cancel native deadline timer",
                                    );
                                    return;
                                }
                                active = None;
                            }
                        }
                        Ok(Command::Shutdown) => {
                            if active.is_some()
                                && unsafe { CancelWaitableTimer(raw_handle(&timer)) } == 0
                            {
                                tracing::warn!(
                                    error = %io::Error::last_os_error(),
                                    "failed to cancel native deadline timer during shutdown"
                                );
                            }
                            return;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
            }
            value if value == WAIT_OBJECT_0 + 1 => {
                let Some(wait) = active.take() else {
                    continue;
                };
                if Instant::now() < wait.deadline {
                    match arm_timer(&timer, wait.deadline) {
                        Ok(()) => active = Some(wait),
                        Err(error) => {
                            active = Some(wait);
                            fail_worker(
                                &mut active,
                                &failed,
                                error,
                                "failed to rearm early native deadline timer",
                            );
                            return;
                        }
                    }
                } else {
                    let _ = wait.completed.send(Ok(()));
                }
            }
            WAIT_FAILED => {
                fail_worker(
                    &mut active,
                    &failed,
                    io::Error::last_os_error(),
                    "native deadline timer wait failed",
                );
                return;
            }
            value => {
                fail_worker(
                    &mut active,
                    &failed,
                    io::Error::other(format!(
                        "native deadline timer returned unexpected wait result {value}"
                    )),
                    "native deadline timer wait failed",
                );
                return;
            }
        }
    }
}

fn arm_timer(timer: &OwnedHandle, deadline: Instant) -> io::Result<()> {
    let due_time = relative_due_time(deadline.saturating_duration_since(Instant::now()))?;
    if unsafe { SetWaitableTimer(raw_handle(timer), &due_time, 0, None, std::ptr::null(), 0) } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn relative_due_time(delay: Duration) -> io::Result<i64> {
    let ticks = delay.as_nanos().div_ceil(100).max(1);
    let ticks = i64::try_from(ticks)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "deadline exceeds timer range"))?;
    Ok(-ticks)
}

fn signal(event: &OwnedHandle) -> io::Result<()> {
    if unsafe { SetEvent(raw_handle(event)) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

fn fail_worker(
    active: &mut Option<ActiveWait>,
    failed: &AtomicBool,
    error: io::Error,
    message: &'static str,
) {
    failed.store(true, Ordering::Release);
    tracing::warn!(%error, "{message}");
    if let Some(wait) = active.take() {
        let _ = wait
            .completed
            .send(Err(io::Error::new(error.kind(), error.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_deadline_rounds_up_to_windows_timer_ticks() {
        assert_eq!(relative_due_time(Duration::ZERO).unwrap(), -1);
        assert_eq!(relative_due_time(Duration::from_nanos(1)).unwrap(), -1);
        assert_eq!(relative_due_time(Duration::from_nanos(100)).unwrap(), -1);
        assert_eq!(relative_due_time(Duration::from_nanos(101)).unwrap(), -2);
    }
}
