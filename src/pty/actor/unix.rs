use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{mpsc as std_mpsc, Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TryRecvError as DataTryRecvError};
use tracing::{debug, warn};

use crate::pty::fd;

// Queue work before waking. Final handle drop closes the wake writer, so HUP
// also wakes an idle or quiesced actor and exposes disconnected commands.
const ACTOR_COMMAND_BUFFER: usize = 1024;
const ACTOR_READ_BUFFER_SIZE: usize = 8192;
const HANDOFF_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorState {
    Running,
    Quiesced,
    Released,
}

pub(crate) struct PtyReadResult {
    pub terminal_responses: Vec<Bytes>,
}

impl PtyReadResult {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            terminal_responses: Vec::new(),
        }
    }
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    rows: u16,
    cols: u16,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PtyResizeRequest {
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
}

#[derive(Default)]
struct SharedPtyControls {
    resize: Option<PtyResizeRequest>,
    nudge: Option<PtyResize>,
    terminal_responses: Vec<Bytes>,
}

pub(crate) struct PtyIoActorConfig {
    pub pane_id: u32,
    pub master_fd: OwnedFd,
    pub initially_quiesced: bool,
    pub on_read: ReadCallback,
    pub on_reader_exit: Option<ReaderExitCallback>,
}

enum PtyIoDataCommand {
    WriteUserInput(Bytes),
    SubmitUserInput {
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        reply: std_mpsc::Sender<std::io::Result<()>>,
    },
}

struct QueuedDataCommand {
    command: PtyIoDataCommand,
    #[cfg(feature = "latency-prof")]
    trace: super::input_trace::CommandTrace,
}

impl From<PtyIoDataCommand> for QueuedDataCommand {
    fn from(command: PtyIoDataCommand) -> Self {
        Self {
            command,
            #[cfg(feature = "latency-prof")]
            trace: Default::default(),
        }
    }
}

enum PtyIoControlCommand {
    BeginHandoff(std_mpsc::Sender<std::io::Result<()>>),
    DuplicateForHandoff(std_mpsc::Sender<std::io::Result<RawFd>>),
    RollbackHandoff(std_mpsc::Sender<std::io::Result<()>>),
    ReleaseAfterCommit(std_mpsc::Sender<std::io::Result<()>>),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct PtyIoActorHandle {
    data_tx: mpsc::Sender<QueuedDataCommand>,
    control_tx: std_mpsc::Sender<PtyIoControlCommand>,
    wake: fd::WakeWriter,
    user_writes: Arc<Mutex<UserWriteGate>>,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    foreground_observer: PtyForegroundObserver,
}

/// Reads the current kernel foreground group without waking or waiting for the
/// actor. The weak file reference cannot retain the PTY after actor teardown.
#[derive(Clone)]
pub(crate) struct PtyForegroundObserver {
    master: Weak<std::fs::File>,
}

impl PtyForegroundObserver {
    pub(crate) fn foreground_process_group_id(&self) -> Option<u32> {
        // Hold ownership only across the ioctl, preventing descriptor reuse while
        // allowing actor shutdown and handoff release to close the master normally.
        let master = self.master.upgrade()?;
        crate::platform::foreground_process_group_id_for_tty_fd(master.as_raw_fd())
    }
}

#[derive(Debug)]
struct UserWriteGate {
    accepting: bool,
    #[cfg(feature = "latency-prof")]
    trace: super::input_trace::InputQueueTrace,
}

impl PtyIoActorHandle {
    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        #[cfg(not(feature = "latency-prof"))]
        self.data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(bytes).into())
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(queued) => {
                    let PtyIoDataCommand::WriteUserInput(bytes) = queued.command else {
                        unreachable!("queued write returned another command")
                    };
                    mpsc::error::TrySendError::Full(bytes)
                }
                mpsc::error::TrySendError::Closed(queued) => {
                    let PtyIoDataCommand::WriteUserInput(bytes) = queued.command else {
                        unreachable!("queued write returned another command")
                    };
                    mpsc::error::TrySendError::Closed(bytes)
                }
            })?;
        #[cfg(feature = "latency-prof")]
        {
            let mut user_writes = user_writes;
            let permit = match self.data_tx.try_reserve() {
                Ok(permit) => permit,
                Err(mpsc::error::TrySendError::Full(())) => {
                    return Err(mpsc::error::TrySendError::Full(bytes))
                }
                Err(mpsc::error::TrySendError::Closed(())) => {
                    return Err(mpsc::error::TrySendError::Closed(bytes))
                }
            };
            let trace = user_writes.trace.accepted(&bytes, None);
            trace.enqueue();
            permit.send(QueuedDataCommand {
                command: PtyIoDataCommand::WriteUserInput(bytes),
                trace,
            });
        }
        self.wake_actor();
        Ok(())
    }

    pub(crate) fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty actor closed",
            ));
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        #[cfg(not(feature = "latency-prof"))]
        self.data_tx
            .try_send(
                PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    reply: reply_tx,
                }
                .into(),
            )
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                }
            })?;
        #[cfg(feature = "latency-prof")]
        {
            let mut user_writes = user_writes;
            let permit = self.data_tx.try_reserve().map_err(|err| match err {
                mpsc::error::TrySendError::Full(()) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                }
                mpsc::error::TrySendError::Closed(()) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                }
            })?;
            let trace = user_writes.trace.accepted(&text, Some(&enter));
            trace.enqueue();
            permit.send(QueuedDataCommand {
                command: PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    reply: reply_tx,
                },
                trace,
            });
        }
        self.wake_actor();
        Ok(reply_rx)
    }

    pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        let _order = self
            .response_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(bytes) = response() else {
            return;
        };
        if !bytes.is_empty() {
            self.controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .terminal_responses
                .push(bytes);
            self.wake_actor();
        }
    }

    pub(crate) fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.resize = Some(PtyResizeRequest {
                resize: PtyResize {
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                },
                terminal_responses,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn nudge_child_redraw_after_handoff(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.nudge = Some(PtyResize {
                rows,
                cols,
                cell_width_px,
                cell_height_px,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn begin_handoff(&self, timeout: Duration) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !user_writes.accepting {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "PTY handoff is already in progress",
                ));
            }
            user_writes.accepting = false;
            if self
                .control_tx
                .send(PtyIoControlCommand::BeginHandoff(reply_tx))
                .is_err()
            {
                user_writes.accepting = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            self.wake_actor();
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                let _ = self.rollback_handoff();
                Err(err)
            }
            Err(_) => {
                let _ = self.rollback_handoff();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for PTY actor to quiesce",
                ))
            }
        }
    }

    pub(crate) fn duplicate_for_handoff(&self) -> std::io::Result<RawFd> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::DuplicateForHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff duplicate",
            )
        })?
    }

    pub(crate) fn foreground_process_group_id(&self) -> Option<u32> {
        self.foreground_observer.foreground_process_group_id()
    }

    pub(crate) fn foreground_observer(&self) -> PtyForegroundObserver {
        self.foreground_observer.clone()
    }

    pub(crate) fn rollback_handoff(&self) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::RollbackHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        let result = reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff rollback",
            )
        })?;
        if result.is_ok() {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = true;
        }
        result
    }

    pub(crate) fn release_after_commit(&self) -> std::io::Result<()> {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ReleaseAfterCommit(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY actor release",
            )
        })?
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        if self.control_tx.send(PtyIoControlCommand::Shutdown).is_ok() {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            debug!(err = %err, "failed to wake PTY actor");
        }
    }
}

pub(crate) struct PtyIoActor;

#[cfg(test)]
#[derive(Debug)]
enum ActorPollEvent {
    Waiting {
        timeout_ms: i32,
    },
    Returned {
        pty_read_ready: bool,
        pty_write_ready: bool,
        wake_ready: bool,
    },
}

#[cfg(test)]
struct ActorPollObserver {
    events: std_mpsc::Sender<ActorPollEvent>,
    resume: Option<std_mpsc::Receiver<()>>,
}

#[cfg(not(test))]
type ActorPollObserver = ();

impl PtyIoActor {
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<ActorPollObserver>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe()?;
        let user_writes = Arc::new(Mutex::new(UserWriteGate {
            accepting: !config.initially_quiesced,
            #[cfg(feature = "latency-prof")]
            trace: Default::default(),
        }));
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let master = Arc::new(std::fs::File::from(config.master_fd));
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            foreground_observer: PtyForegroundObserver {
                master: Arc::downgrade(&master),
            },
        };

        let mut runner = PtyIoActorRunner {
            diagnostic_input: crate::latency_prof::InputStimuli::default(),
            pane_id: config.pane_id,
            file: master,
            read_buffer: [0; ACTOR_READ_BUFFER_SIZE],
            data_rx,
            control_rx,
            state: if config.initially_quiesced {
                ActorState::Quiesced
            } else {
                ActorState::Running
            },
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls,
            response_order,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            poll_observer,
        };
        std::thread::Builder::new()
            .name(format!("herdr-pty-{}", config.pane_id))
            .spawn(move || runner.run())
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        Ok(handle)
    }

    #[cfg(test)]
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: ActorPollObserver,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, Some(poll_observer))
    }
}

struct PtyIoActorRunner {
    pane_id: u32,
    file: Arc<std::fs::File>,
    // Reuse initialized storage across normal reads and pre-handoff draining.
    read_buffer: [u8; ACTOR_READ_BUFFER_SIZE],
    data_rx: mpsc::Receiver<QueuedDataCommand>,
    control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
    state: ActorState,
    diagnostic_input: crate::latency_prof::InputStimuli,
    pending_writes: VecDeque<PendingWrite>,
    current_write_offset: usize,
    active_submission: Option<ActiveSubmission>,
    pending_handoff: Option<std_mpsc::Sender<std::io::Result<()>>>,
    wake_read_fd: OwnedFd,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    #[cfg_attr(not(test), allow(dead_code))] // Observer exists only in test runners.
    poll_observer: Option<ActorPollObserver>,
}

struct ActiveSubmission {
    enter: Bytes,
    delay: Duration,
    phase: SubmissionPhase,
    #[cfg(feature = "latency-prof")]
    enter_trace: super::input_trace::PartTrace,
    reply: std_mpsc::Sender<std::io::Result<()>>,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingWrite {
    bytes: Bytes,
    boundary: Option<SubmissionBoundary>,
    #[cfg(feature = "latency-prof")]
    trace: super::input_trace::PartTrace,
    #[cfg(feature = "latency-prof")]
    attempted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionBoundary {
    Text,
    Enter,
}

enum SubmissionPhase {
    WritingText,
    WaitingUntil(Instant),
    WritingEnter,
}

impl PtyIoActorRunner {
    #[cfg(feature = "latency-prof")]
    fn trace_last_write(&mut self, trace: super::input_trace::PartTrace) {
        if trace.is_empty() {
            return;
        }
        if let Some(write) = self.pending_writes.back_mut() {
            if write.trace == Default::default() {
                write.trace = trace;
                trace.record("input.pty_pending");
            }
        }
    }

    fn discard_pending_writes(&mut self) {
        #[cfg(feature = "latency-prof")]
        for write in &self.pending_writes {
            write.trace.record("input.pty_part_discard");
        }
        self.pending_writes.clear();
    }

    fn enqueue_write(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending_writes.push_back(PendingWrite {
                bytes,
                boundary: None,
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
                #[cfg(feature = "latency-prof")]
                attempted: false,
            });
        }
    }

    fn enqueue_submission_write(&mut self, bytes: Bytes, boundary: SubmissionBoundary) {
        if !bytes.is_empty() {
            self.pending_writes.push_back(PendingWrite {
                bytes,
                boundary: Some(boundary),
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
                #[cfg(feature = "latency-prof")]
                attempted: false,
            });
        }
    }

    fn run(&mut self) {
        let mut should_exit = false;
        while !should_exit {
            should_exit = self.drain_commands();
            if should_exit || self.state == ActorState::Released {
                break;
            }

            self.apply_pending_controls();

            if !self.pending_writes.is_empty() {
                match self.flush_pending_writes_once() {
                    Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                    Ok(None) => {}
                    Err(err) => {
                        self.fail_active_submission(err);
                        break;
                    }
                }
            }
            self.schedule_submission_enter();
            if self.active_submission.is_none() && self.pending_handoff.is_some() {
                continue;
            }

            let timeout_ms = self.poll_timeout_ms();
            #[cfg(test)]
            if let Some(observer) = &self.poll_observer {
                let _ = observer.events.send(ActorPollEvent::Waiting { timeout_ms });
                if let Some(resume) = &observer.resume {
                    let _ = resume.recv();
                }
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                self.state == ActorState::Running,
                !self.pending_writes.is_empty(),
                timeout_ms,
            ) {
                Ok(readiness) => {
                    #[cfg(test)]
                    if let Some(observer) = &self.poll_observer {
                        let _ = observer.events.send(ActorPollEvent::Returned {
                            pty_read_ready: readiness.pty_read_ready,
                            pty_write_ready: readiness.pty_write_ready,
                            wake_ready: readiness.wake_ready,
                        });
                    }
                    if readiness.wake_ready {
                        if let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd()) {
                            debug!(pane = self.pane_id, err = %err, "PTY actor wake drain failed");
                            break;
                        }
                        continue;
                    }
                    if self.state == ActorState::Running
                        && readiness.pty_read_ready
                        && !self.read_once()
                    {
                        break;
                    }
                    if readiness.pty_write_ready && !self.pending_writes.is_empty() {
                        match self.flush_pending_writes_once() {
                            Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                            Ok(None) => {}
                            Err(err) => {
                                self.fail_active_submission(err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    debug!(pane = self.pane_id, err = %err, "PTY actor poll failed");
                    break;
                }
            }
        }

        self.close_input_queue();
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit();
        }
        debug!(pane = self.pane_id, "PTY actor exiting");
    }

    fn drain_commands(&mut self) -> bool {
        if self.drain_control_commands() {
            return true;
        }
        if self.active_submission.is_some() {
            return false;
        }
        if let Some(reply) = self.pending_handoff.take() {
            self.defer_or_begin_handoff(reply);
            return false;
        }
        self.drain_data_commands()
    }

    fn drain_control_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.control_rx.try_recv() {
                Ok(command) => {
                    if self.handle_control_command(command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn drain_data_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.data_rx.try_recv() {
                Ok(command) => {
                    if self.handle_data_command(command) {
                        should_exit = true;
                        break;
                    }
                    if self.active_submission.is_some() {
                        break;
                    }
                }
                Err(DataTryRecvError::Empty) => break,
                Err(DataTryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn handle_data_command(&mut self, queued: impl Into<QueuedDataCommand>) -> bool {
        let queued = queued.into();
        #[cfg(feature = "latency-prof")]
        queued.trace.claim();
        if self.state != ActorState::Running {
            #[cfg(feature = "latency-prof")]
            queued.trace.discard();
        }
        match queued.command {
            PtyIoDataCommand::WriteUserInput(bytes) => {
                if self.state == ActorState::Running {
                    self.enqueue_write(bytes);
                    #[cfg(feature = "latency-prof")]
                    self.trace_last_write(queued.trace.text);
                }
            }
            PtyIoDataCommand::SubmitUserInput {
                text,
                enter,
                delay,
                reply,
            } => {
                if self.state == ActorState::Running {
                    let phase = if text.is_empty() {
                        SubmissionPhase::WaitingUntil(Instant::now() + delay)
                    } else {
                        self.enqueue_submission_write(text, SubmissionBoundary::Text);
                        #[cfg(feature = "latency-prof")]
                        self.trace_last_write(queued.trace.text);
                        SubmissionPhase::WritingText
                    };
                    self.active_submission = Some(ActiveSubmission {
                        enter,
                        delay,
                        phase,
                        #[cfg(feature = "latency-prof")]
                        enter_trace: queued.trace.enter,
                        reply,
                    });
                } else {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "pty actor is not accepting input",
                    )));
                }
            }
        }
        false
    }

    fn handle_control_command(&mut self, command: PtyIoControlCommand) -> bool {
        match command {
            PtyIoControlCommand::BeginHandoff(reply) => {
                self.defer_or_begin_handoff(reply);
            }
            PtyIoControlCommand::DuplicateForHandoff(reply) => {
                let result = if self.state == ActorState::Quiesced {
                    fd::duplicate_cloexec_fd(self.file.as_raw_fd())
                } else {
                    Err(std::io::Error::other(
                        "PTY actor must be quiesced before handoff duplication",
                    ))
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::RollbackHandoff(reply) => {
                self.pending_handoff.take();
                let result = if self.state == ActorState::Released {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "PTY actor was released before handoff rollback",
                    ))
                } else {
                    self.state = ActorState::Running;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ReleaseAfterCommit(reply) => {
                self.state = ActorState::Released;
                self.discard_pending_writes();
                let _ = reply.send(Ok(()));
                return true;
            }
            PtyIoControlCommand::Shutdown => return true,
        }
        false
    }

    fn defer_or_begin_handoff(&mut self, reply: std_mpsc::Sender<std::io::Result<()>>) {
        if self.active_submission.is_none() {
            self.drain_pre_quiesce_commands();
        }
        if self.active_submission.is_some() {
            self.pending_handoff = Some(reply);
        } else {
            let result = self.begin_handoff();
            let _ = reply.send(result);
        }
    }

    fn begin_handoff(&mut self) -> std::io::Result<()> {
        self.drain_pre_quiesce_commands();
        if self.active_submission.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "PTY input submission is still in progress",
            ));
        }
        self.apply_pending_controls();
        if self.state == ActorState::Released {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "PTY actor was released before handoff quiesce",
            ));
        }
        let deadline = Instant::now() + HANDOFF_DRAIN_TIMEOUT;
        let _ = self.flush_pending_writes_once()?;
        while !self.pending_writes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out draining PTY writes before handoff",
                ));
            }
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let readiness = fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                true,
                true,
                timeout_ms,
            )?;
            if readiness.wake_ready {
                fd::drain_wake_fd(self.wake_read_fd.as_raw_fd())?;
            }
            if readiness.pty_read_ready && !self.read_once() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "PTY closed while draining writes before handoff",
                ));
            }
            if readiness.pty_write_ready {
                let _ = self.flush_pending_writes_once()?;
            }
        }
        self.state = ActorState::Quiesced;
        Ok(())
    }

    fn drain_pre_quiesce_commands(&mut self) {
        while let Ok(command) = self.data_rx.try_recv() {
            if self.handle_data_command(command) {
                break;
            }
            if self.active_submission.is_some() {
                break;
            }
        }
    }

    fn apply_pending_controls(&mut self) {
        let (resize, nudge, terminal_responses) = {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                controls.resize.take(),
                controls.nudge.take(),
                std::mem::take(&mut controls.terminal_responses),
            )
        };
        if self.state == ActorState::Released {
            return;
        }
        if let Some(request) = resize {
            self.resize(request.resize);
            self.enqueue_terminal_responses(request.terminal_responses);
        }
        if let Some(nudge) = nudge {
            self.nudge(nudge);
        }
        self.enqueue_terminal_responses(terminal_responses);
    }

    fn read_once(&mut self) -> bool {
        crate::latency_prof::zone!("pty.read_batch");
        match self.file.as_ref().read(&mut self.read_buffer) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
            Err(err) => {
                debug!(pane = self.pane_id, err = %err, "PTY actor read failed");
                false
            }
            Ok(n) => {
                crate::latency_prof::record("pty.read", self.pane_id as u64, n as u64);
                let response_order = Arc::clone(&self.response_order);
                let _order = response_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = (self.on_read)(&self.read_buffer[..n]);
                self.controls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .terminal_responses
                    .extend(result.terminal_responses);
                drop(_order);
                let terminal_responses = std::mem::take(
                    &mut self
                        .controls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .terminal_responses,
                );
                self.enqueue_terminal_responses(terminal_responses);
                true
            }
        }
    }

    fn enqueue_terminal_responses(&mut self, terminal_responses: Vec<Bytes>) {
        if self.state == ActorState::Released {
            return;
        }
        for bytes in terminal_responses {
            self.enqueue_write(bytes);
        }
    }

    fn complete_submission_boundary(&mut self, boundary: SubmissionBoundary) {
        match boundary {
            SubmissionBoundary::Text => {
                let Some(submission) = self.active_submission.as_mut() else {
                    return;
                };
                debug_assert!(matches!(submission.phase, SubmissionPhase::WritingText));
                submission.phase = SubmissionPhase::WaitingUntil(Instant::now() + submission.delay);
            }
            SubmissionBoundary::Enter => {
                let Some(submission) = self.active_submission.take() else {
                    return;
                };
                debug_assert!(matches!(submission.phase, SubmissionPhase::WritingEnter));
                let _ = submission.reply.send(Ok(()));
            }
        }
    }

    fn schedule_submission_enter(&mut self) {
        let Some(ActiveSubmission {
            enter,
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return;
        };
        if Instant::now() >= *deadline {
            let enter = enter.clone();
            if enter.is_empty() {
                let submission = self.active_submission.take().unwrap();
                let _ = submission.reply.send(Ok(()));
            } else {
                self.active_submission.as_mut().unwrap().phase = SubmissionPhase::WritingEnter;
                #[cfg(feature = "latency-prof")]
                let enter_trace = self
                    .active_submission
                    .as_ref()
                    .map(|submission| submission.enter_trace)
                    .unwrap_or_default();
                self.enqueue_submission_write(enter, SubmissionBoundary::Enter);
                #[cfg(feature = "latency-prof")]
                self.trace_last_write(enter_trace);
            }
        }
    }

    fn poll_timeout_ms(&self) -> i32 {
        let Some(ActiveSubmission {
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return -1;
        };
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1)
            .min(i32::MAX as u128) as i32
    }

    fn fail_active_submission(&mut self, err: std::io::Error) {
        if let Some(submission) = self.active_submission.take() {
            #[cfg(feature = "latency-prof")]
            submission.enter_trace.record("input.pty_part_discard");
            let _ = submission.reply.send(Err(err));
        }
    }

    fn close_input_queue(&mut self) {
        self.data_rx.close();
        self.discard_pending_writes();
        self.fail_active_submission(input_submission_closed_error());
        while let Some(command) = self.data_rx.blocking_recv() {
            #[cfg(feature = "latency-prof")]
            command.trace.discard();
            if let PtyIoDataCommand::SubmitUserInput { reply, .. } = command.command {
                let _ = reply.send(Err(input_submission_closed_error()));
            }
        }
    }

    fn flush_pending_writes_once(&mut self) -> std::io::Result<Option<SubmissionBoundary>> {
        crate::latency_prof::zone!("pty.write_batch");
        crate::latency_prof::record(
            "pty.write_start",
            self.pane_id as u64,
            self.pending_writes.len() as u64,
        );
        while let Some(write) = self.pending_writes.front_mut() {
            #[cfg(feature = "latency-prof")]
            if !write.attempted {
                write.trace.record("input.pty_write_attempt");
                write.attempted = true;
            }
            let chunk = &write.bytes[self.current_write_offset..];
            match self.file.as_ref().write(chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "PTY actor write returned zero bytes",
                    ));
                }
                Ok(written) => {
                    self.current_write_offset += written;
                    if self.current_write_offset >= write.bytes.len() {
                        let completed = self.pending_writes.pop_front().unwrap();
                        #[cfg(feature = "latency-prof")]
                        completed.trace.record("input.pty_part_complete");
                        self.diagnostic_input.observe(
                            &completed.bytes,
                            "input.pty_write_complete",
                            self.pane_id as u64,
                        );
                        self.current_write_offset = 0;
                        if let Some(boundary) = completed.boundary {
                            self.file.as_ref().flush()?;
                            return Ok(Some(boundary));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
                Err(err) => {
                    warn!(pane = self.pane_id, err = %err, "PTY actor write failed");
                    self.discard_pending_writes();
                    self.current_write_offset = 0;
                    return Err(err);
                }
            }
        }
        self.file.as_ref().flush()?;
        Ok(None)
    }

    fn resize(&self, resize: PtyResize) {
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn nudge(&mut self, resize: PtyResize) {
        if self.state == ActorState::Released {
            return;
        }
        let nudge = if resize.rows > 2 {
            (
                resize.rows - 1,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        } else {
            (
                resize.rows,
                resize.cols.saturating_sub(1).max(4),
                resize.cell_width_px,
                resize.cell_height_px,
            )
        };
        if nudge
            == (
                resize.rows,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        {
            return;
        }
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            nudge.0,
            nudge.1,
            nudge.2,
            nudge.3,
        ));
        std::thread::sleep(Duration::from_millis(30));
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn log_resize_result(&self, result: std::io::Result<()>) {
        if let Err(err) = result {
            debug!(pane = self.pane_id, err = %err, "PTY resize failed");
        }
    }
}

fn input_submission_closed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "PTY actor closed during input submission",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };

    fn test_wake_pair() -> (fd::WakeWriter, OwnedFd) {
        let pipe = fd::create_wake_pipe().expect("wake pipe");
        (pipe.writer, pipe.read_fd)
    }

    fn actor_with_socket_pair(
        initially_quiesced: bool,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        actor_with_socket_pair_and_poll_observer(initially_quiesced, None)
    }

    fn actor_with_socket_pair_and_poll_observer(
        initially_quiesced: bool,
        poll_observer: Option<ActorPollObserver>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (_data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let runner = PtyIoActorRunner {
            diagnostic_input: crate::latency_prof::InputStimuli::default(),
            pane_id: 1,
            file: Arc::new(std::fs::File::from(owned)),
            read_buffer: [0; ACTOR_READ_BUFFER_SIZE],
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };
        (runner, peer)
    }

    fn paused_actor(
        initially_quiesced: bool,
    ) -> (
        PtyIoActorHandle,
        UnixStream,
        std_mpsc::Receiver<Bytes>,
        std_mpsc::Receiver<ActorPollEvent>,
        std_mpsc::Sender<()>,
    ) {
        let (event_tx, event_rx) = std_mpsc::channel();
        let (resume_tx, resume_rx) = std_mpsc::channel();
        let (handle, peer, read_rx) = actor_with_socket_pair_and_poll_observer(
            initially_quiesced,
            Some(ActorPollObserver {
                events: event_tx,
                resume: Some(resume_rx),
            }),
        );
        (handle, peer, read_rx, event_rx, resume_tx)
    }

    fn expect_actor_wait(events: &std_mpsc::Receiver<ActorPollEvent>) -> i32 {
        match events
            .recv_timeout(Duration::from_secs(3))
            .expect("actor reaches poll boundary")
        {
            ActorPollEvent::Waiting { timeout_ms } => timeout_ms,
            event => panic!("expected poll boundary, received {event:?}"),
        }
    }

    fn expect_actor_wake(events: &std_mpsc::Receiver<ActorPollEvent>) {
        match events
            .recv_timeout(Duration::from_secs(3))
            .expect("actor returns from poll")
        {
            ActorPollEvent::Returned {
                wake_ready: true, ..
            } => {}
            event => panic!("expected explicit wake readiness, received {event:?}"),
        }
    }

    #[test]
    fn final_handle_drop_wakes_running_and_quiesced_actors() {
        for initially_quiesced in [false, true] {
            let (handle, mut peer, _read_rx, events, resume) = paused_actor(initially_quiesced);
            let observer = handle.foreground_observer();
            let clone = handle.clone();
            expect_actor_wait(&events);
            drop(handle);
            assert!(observer.master.upgrade().is_some());
            drop(clone);
            resume.send(()).expect("release actor poll boundary");
            expect_actor_wake(&events);
            assert!(matches!(
                events.recv_timeout(Duration::from_secs(3)),
                Err(std_mpsc::RecvTimeoutError::Disconnected)
            ));
            assert_eq!(
                peer.read(&mut [0; 1]).expect("actor closes its descriptor"),
                0
            );
            assert!(observer.master.upgrade().is_none());
        }
    }

    #[test]
    fn idle_running_and_quiesced_actors_have_no_poll_deadline() {
        for initially_quiesced in [false, true] {
            let (handle, _peer, _read_rx, events, resume) = paused_actor(initially_quiesced);
            let timeout_ms = expect_actor_wait(&events);
            handle.shutdown();
            drop(resume);
            assert_eq!(timeout_ms, -1, "idle actor waits for explicit readiness");
        }
        let (mut runner, _peer) = actor_runner_for_unit_test();
        let (reply, _completion) = std_mpsc::channel();
        runner.active_submission = Some(ActiveSubmission {
            enter: Bytes::from_static(b"\r"),
            delay: Duration::from_secs(30),
            phase: SubmissionPhase::WaitingUntil(Instant::now() + Duration::from_secs(30)),
            #[cfg(feature = "latency-prof")]
            enter_trace: Default::default(),
            reply,
        });
        assert!(
            runner.poll_timeout_ms() > 1000,
            "real submission deadline has no idle-poll cap"
        );
    }

    #[test]
    fn queued_input_and_response_wake_before_actor_enters_poll() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        handle
            .try_write_user_input(Bytes::from_static(b"input"))
            .expect("input accepted");
        handle.write_terminal_response(|| Some(Bytes::from_static(b"reply")));
        resume.send(()).expect("release actor poll boundary");
        expect_actor_wake(&events);
        expect_actor_wait(&events);
        let mut received = [0; 10];
        peer.read_exact(&mut received)
            .expect("peer receives queued writes");
        assert_eq!(&received, b"inputreply");
        handle.shutdown();
        drop(resume);
    }

    #[test]
    fn resize_and_nudge_wake_before_actor_enters_poll() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        handle.resize(20, 80, 8, 16, vec![Bytes::from_static(b"resize")]);
        handle.nudge_child_redraw_after_handoff(20, 80, 8, 16);
        resume.send(()).expect("release actor poll boundary");
        expect_actor_wake(&events);
        expect_actor_wait(&events);
        let mut received = [0; 6];
        peer.read_exact(&mut received)
            .expect("peer receives resize response");
        assert_eq!(&received, b"resize");
        handle.shutdown();
        drop(resume);
    }

    #[test]
    fn shutdown_wakes_running_and_quiesced_actors() {
        for initially_quiesced in [false, true] {
            let (handle, mut peer, _read_rx, events, resume) = paused_actor(initially_quiesced);
            expect_actor_wait(&events);
            handle.shutdown();
            resume.send(()).expect("release actor poll boundary");
            expect_actor_wake(&events);
            assert!(matches!(
                events.recv_timeout(Duration::from_secs(3)),
                Err(std_mpsc::RecvTimeoutError::Disconnected)
            ));
            assert_eq!(peer.read(&mut [0; 1]).expect("actor closes master"), 0);
        }
    }

    #[test]
    fn concurrent_input_and_nonfinal_drop_preserve_delivery() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        let writer = handle.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            writer
                .try_write_user_input(Bytes::from_static(b"concurrent"))
                .expect("live clone accepts input");
            writer
        });
        barrier.wait();
        drop(handle);
        let writer = worker.join().expect("input worker joins");
        resume.send(()).expect("release actor poll boundary");
        expect_actor_wake(&events);
        expect_actor_wait(&events);
        let mut received = [0; 10];
        peer.read_exact(&mut received)
            .expect("peer receives concurrent input");
        assert_eq!(&received, b"concurrent");
        drop(writer);
        resume.send(()).expect("release final poll boundary");
        expect_actor_wake(&events);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)),
            Err(std_mpsc::RecvTimeoutError::Disconnected)
        ));
        assert_eq!(peer.read(&mut [0; 1]).expect("final drop closes master"), 0);
    }

    #[test]
    fn rollback_and_release_wake_quiesced_actor() {
        let (handle, mut peer, read_rx, events, resume) = paused_actor(true);
        expect_actor_wait(&events);
        peer.write_all(b"held")
            .expect("peer queues output while quiesced");
        let rollback_handle = handle.clone();
        let rollback = std::thread::spawn(move || rollback_handle.rollback_handoff());
        resume.send(()).expect("release rollback poll boundary");
        expect_actor_wake(&events);
        rollback
            .join()
            .expect("rollback worker joins")
            .expect("rollback resumes actor");
        expect_actor_wait(&events);
        resume.send(()).expect("allow actor to read held output");
        match events
            .recv_timeout(Duration::from_secs(3))
            .expect("PTY readiness")
        {
            ActorPollEvent::Returned {
                pty_read_ready: true,
                pty_write_ready: false,
                wake_ready: false,
            } => {}
            event => panic!("expected held PTY output readiness, received {event:?}"),
        }
        assert_eq!(
            read_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("held output delivered"),
            Bytes::from_static(b"held")
        );
        expect_actor_wait(&events);
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));
        resume.send(()).expect("release handoff poll boundary");
        expect_actor_wake(&events);
        handoff
            .join()
            .expect("handoff worker joins")
            .expect("actor quiesces");
        expect_actor_wait(&events);
        let release_handle = handle.clone();
        let release = std::thread::spawn(move || release_handle.release_after_commit());
        resume.send(()).expect("release commit poll boundary");
        expect_actor_wake(&events);
        release
            .join()
            .expect("release worker joins")
            .expect("actor releases");
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)),
            Err(std_mpsc::RecvTimeoutError::Disconnected)
        ));
        assert_eq!(peer.read(&mut [0; 1]).expect("release closes master"), 0);
    }

    #[test]
    fn handoff_duplicate_wakes_quiesced_actor_and_remains_owned() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(true);
        expect_actor_wait(&events);
        let duplicate_handle = handle.clone();
        let worker = std::thread::spawn(move || duplicate_handle.duplicate_for_handoff());
        resume.send(()).expect("release duplicate poll boundary");
        expect_actor_wake(&events);
        let duplicate = worker
            .join()
            .expect("duplicate worker joins")
            .expect("handoff descriptor duplicated");
        let mut duplicate = unsafe { std::fs::File::from_raw_fd(duplicate) };
        duplicate
            .write_all(b"duplicate")
            .expect("duplicated descriptor stays usable");
        let mut received = [0; 9];
        peer.read_exact(&mut received)
            .expect("peer receives duplicate output");
        assert_eq!(&received, b"duplicate");
        handle.shutdown();
        drop(resume);
        drop(duplicate);
    }

    #[test]
    fn full_input_queue_does_not_block_control_wakes_or_handoff() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        for _ in 0..ACTOR_COMMAND_BUFFER {
            handle
                .try_write_user_input(Bytes::from_static(b"x"))
                .expect("queue accepts input");
        }
        assert!(matches!(
            handle.try_write_user_input(Bytes::from_static(b"overflow")),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        handle.resize(20, 80, 8, 16, vec![Bytes::from_static(b"resize")]);
        handle.nudge_child_redraw_after_handoff(20, 80, 8, 16);
        handle.write_terminal_response(|| Some(Bytes::from_static(b"reply")));
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));
        resume.send(()).expect("release full-queue poll boundary");
        expect_actor_wake(&events);
        drop(resume);
        let mut received = vec![0; ACTOR_COMMAND_BUFFER + 11];
        peer.read_exact(&mut received)
            .expect("pre-handoff bytes reach peer");
        assert!(received[..ACTOR_COMMAND_BUFFER]
            .iter()
            .all(|byte| *byte == b'x'));
        assert_eq!(&received[ACTOR_COMMAND_BUFFER..], b"resizereply");
        handoff
            .join()
            .expect("handoff worker joins")
            .expect("full queue handoff quiesces");
        handle.shutdown();
    }

    #[test]
    fn delayed_enter_uses_deadline_readiness_without_another_command() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_millis(40),
            )
            .expect("delayed submission accepted");
        resume.send(()).expect("release submission poll boundary");
        expect_actor_wake(&events);
        let deadline_timeout = expect_actor_wait(&events);
        assert!((1..=40).contains(&deadline_timeout));
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("peer receives completed prompt");
        assert_eq!(&prompt, b"prompt");
        resume.send(()).expect("allow finite deadline poll");
        match events
            .recv_timeout(Duration::from_secs(3))
            .expect("deadline expires")
        {
            ActorPollEvent::Returned {
                pty_read_ready: false,
                pty_write_ready: false,
                wake_ready: false,
            } => {}
            event => panic!("expected deadline return without I/O, received {event:?}"),
        }
        drop(resume);
        let mut enter = [0; 1];
        peer.read_exact(&mut enter)
            .expect("deadline sends delayed Enter");
        assert_eq!(&enter, b"\r");
        completion
            .recv_timeout(Duration::from_secs(3))
            .expect("submission completes")
            .expect("submission succeeds");
        handle.shutdown();
    }

    #[test]
    fn final_drop_cancels_delayed_submission_through_wake_hup() {
        let (handle, mut peer, _read_rx, events, resume) = paused_actor(false);
        expect_actor_wait(&events);
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(5),
            )
            .expect("submission accepted");
        resume.send(()).expect("release submission poll boundary");
        expect_actor_wake(&events);
        expect_actor_wait(&events);
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("prompt reaches peer");
        assert_eq!(&prompt, b"prompt");
        drop(handle);
        resume
            .send(())
            .expect("allow HUP to wake delayed submission");
        expect_actor_wake(&events);
        let err = completion
            .recv_timeout(Duration::from_secs(3))
            .expect("actor reports canceled submission")
            .expect_err("final drop cancels pending Enter");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            peer.read(&mut [0; 1]).expect("actor closes without Enter"),
            0
        );
    }

    #[test]
    fn interrupted_indefinite_poll_waits_for_explicit_wake() {
        // Signal disposition is process-wide; isolate even under cargo test's
        // parallel runner, rather than changing another test's signal behavior.
        if std::env::var_os("HERDR_TEST_POLL_EINTR").is_none() {
            let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "pty::actor::unix::tests::interrupted_indefinite_poll_waits_for_explicit_wake",
                    "--nocapture",
                ])
                .env("HERDR_TEST_POLL_EINTR", "1")
                .status()
                .expect("isolated signal test starts");
            assert!(status.success());
            return;
        }
        unsafe extern "C" fn ignore_signal(_: libc::c_int) {}

        struct RestoreSignal(libc::sigaction);
        impl Drop for RestoreSignal {
            fn drop(&mut self) {
                unsafe { libc::sigaction(libc::SIGUSR1, &self.0, std::ptr::null_mut()) };
            }
        }

        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = ignore_signal as *const () as libc::sighandler_t;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGUSR1, &action, &mut previous) },
            0
        );
        let _restore_signal = RestoreSignal(previous);

        let (thread_tx, thread_rx) = std_mpsc::channel();
        let (interrupt_tx, interrupt_rx) = std_mpsc::channel();
        let (done_tx, done_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let worker = std::thread::spawn(move || {
            fd::INTERRUPTED_POLL_OBSERVER
                .with_borrow_mut(|observer| *observer = Some(interrupt_tx));
            thread_tx
                .send(unsafe { libc::pthread_self() })
                .expect("report poll thread");
            let readiness =
                fd::poll_pty_and_wake(-1, wake_pipe.read_fd.as_raw_fd(), false, false, -1)
                    .expect("interrupted indefinite poll succeeds");
            done_tx
                .send(readiness.wake_ready)
                .expect("report poll readiness");
        });
        let thread_id = thread_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("poll thread starts");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert_eq!(unsafe { libc::pthread_kill(thread_id, libc::SIGUSR1) }, 0);
            match interrupt_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => break,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "signal interrupts blocking poll");
                }
                Err(err) => panic!("interrupt observer closes unexpectedly: {err}"),
            }
        }
        assert!(matches!(
            done_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
        wake_pipe.writer.wake().expect("explicit wake succeeds");
        assert!(done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("poll returns for wake"));
        worker.join().expect("poll thread exits");
    }

    #[test]
    fn handoff_drain_rejects_peer_backpressure_at_its_deadline() {
        let (mut runner, _peer) = actor_runner_for_unit_test();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        runner.wake_read_fd = wake_pipe.read_fd;
        let _wake_writer = wake_pipe.writer;
        let bytes = [b'x'; 8192];
        loop {
            match runner.file.as_ref().write(&bytes) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("socket prefill fails unexpectedly: {err}"),
            }
        }
        runner.enqueue_write(Bytes::from_static(b"pending"));

        let err = runner
            .begin_handoff()
            .expect_err("backpressured handoff cannot quiesce");

        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(runner.state, ActorState::Running);
    }

    #[test]
    fn actor_ignores_empty_user_input_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();

        assert!(!runner.handle_data_command(PtyIoDataCommand::WriteUserInput(Bytes::new())));

        assert!(runner.pending_writes.is_empty());
    }

    #[test]
    fn foreground_observation_does_not_queue_or_wake_actor() {
        let (actor_socket, _peer) = UnixStream::pair().expect("socket pair");
        let master = Arc::new(std::fs::File::from(unsafe {
            OwnedFd::from_raw_fd(actor_socket.into_raw_fd())
        }));
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate {
                accepting: true,
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
            })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            foreground_observer: PtyForegroundObserver {
                master: Arc::downgrade(&master),
            },
        };

        for _ in 0..64 {
            assert_eq!(handle.foreground_process_group_id(), None);
            assert_eq!(
                handle.foreground_observer().foreground_process_group_id(),
                None
            );
        }

        assert!(matches!(
            control_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
        let mut wake = std::fs::File::from(wake_pipe.read_fd);
        assert_eq!(
            wake.read(&mut [0; 1])
                .expect_err("observation leaves wake pipe empty")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(Arc::strong_count(&master), 1);
    }

    #[test]
    fn foreground_observer_does_not_retain_master_after_shutdown_or_release() {
        for release in [false, true] {
            let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
            let observer = handle.foreground_observer();
            if release {
                handle.release_after_commit().expect("actor released");
            } else {
                handle.shutdown();
            }

            assert_eq!(peer.read(&mut [0; 1]).expect("actor closes master"), 0);
            assert!(observer.master.upgrade().is_none());
            assert_eq!(observer.foreground_process_group_id(), None);
            assert_eq!(handle.foreground_process_group_id(), None);
        }
    }

    #[test]
    fn foreground_observer_tracks_live_pty_job_transitions() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "set -m; printf 'ready\\n'; read -r line; sleep 30; printf 'returned\\n'; read -r line",
        ]);
        let mut spawned = crate::pty::backend::spawn_with_portable_pty(24, 80, command)
            .expect("shell starts with controlling tty");
        let shell_pid = spawned.child.process_id().expect("shell pid");
        let (output_tx, output_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: 1,
            master_fd: spawned.master_fd,
            initially_quiesced: false,
            on_read: Box::new(move |bytes| {
                let _ = output_tx.send(Bytes::copy_from_slice(bytes));
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        })
        .expect("actor starts");
        let observer = handle.foreground_observer();
        let wait_for_output = |marker: &str| {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut output = Vec::new();
            while !String::from_utf8_lossy(&output).contains(marker) {
                output.extend(
                    output_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .expect("shell reaches requested state"),
                );
            }
        };
        wait_for_output("ready");
        assert_eq!(observer.foreground_process_group_id(), Some(shell_pid));
        handle
            .try_write_user_input(Bytes::from_static(b"go\r"))
            .expect("shell input accepted");

        let deadline = Instant::now() + Duration::from_secs(2);
        let foreground_pid = loop {
            if let Some(pgid) = observer.foreground_process_group_id() {
                if pgid != shell_pid {
                    break pgid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "foreground sleep acquires terminal"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("actor quiesces");
        assert_eq!(observer.foreground_process_group_id(), Some(foreground_pid));
        assert_eq!(
            unsafe { libc::kill(-(foreground_pid as i32), libc::SIGTERM) },
            0
        );
        handle.rollback_handoff().expect("actor resumes");
        wait_for_output("returned");
        assert_eq!(observer.foreground_process_group_id(), Some(shell_pid));

        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        handle.shutdown();
    }

    #[test]
    fn submission_boundary_does_not_wait_for_following_protocol_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();
        runner.enqueue_submission_write(Bytes::from_static(b"prompt"), SubmissionBoundary::Text);
        runner.enqueue_write(Bytes::from_static(b"response"));

        assert_eq!(
            runner.flush_pending_writes_once().unwrap(),
            Some(SubmissionBoundary::Text)
        );
        assert_eq!(
            runner.pending_writes[0].bytes,
            Bytes::from_static(b"response")
        );
    }

    #[test]
    fn actor_writes_user_input_to_owned_fd() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);

        handle
            .try_write_user_input(Bytes::from_static(b"hello"))
            .expect("write command accepted");

        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives write");
        assert_eq!(&buf, b"hello");
        handle.shutdown();
    }

    #[cfg(feature = "latency-prof")]
    fn trace_scenario(name: &str, child: impl FnOnce()) -> Vec<serde_json::Value> {
        if std::env::var("HERDR_ACTOR_TRACE_SCENARIO").as_deref() == Ok(name) {
            child();
            crate::latency_prof::shutdown();
            return Vec::new();
        }
        let directory = std::env::temp_dir().join(format!(
            "herdr-actor-trace-{}-{}",
            std::process::id(),
            name.rsplit("::").next().expect("test name")
        ));
        std::fs::create_dir(&directory).expect("trace directory");
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([name, "--exact", "--nocapture"])
            .env("HERDR_ACTOR_TRACE_SCENARIO", name)
            .env("HERDR_LATENCY_TRACE_DIR", &directory)
            .env_remove("HERDR_TRACY")
            .status()
            .expect("trace scenario starts");
        assert!(status.success(), "trace scenario passes");
        let records = std::fs::read_dir(&directory)
            .expect("trace files")
            .flat_map(|entry| {
                std::fs::read_to_string(entry.expect("trace file").path())
                    .expect("trace content")
                    .lines()
                    .map(|line| serde_json::from_str(line).expect("trace JSON"))
                    .collect::<Vec<_>>()
            })
            .collect();
        std::fs::remove_dir_all(directory).expect("trace cleanup");
        records
    }

    #[cfg(feature = "latency-prof")]
    #[test]
    fn actor_trace_preserves_fragmented_and_batched_echo() {
        const NAME: &str =
            "pty::actor::unix::tests::actor_trace_preserves_fragmented_and_batched_echo";
        let records = trace_scenario(NAME, || {
            let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
            for fragment in [
                b"!000000".as_slice(),
                b"000001~!000000000002~!000000000003~".as_slice(),
            ] {
                handle
                    .try_write_user_input(Bytes::copy_from_slice(fragment))
                    .expect("fragment accepted");
            }
            let mut received = [0; 42];
            peer.read_exact(&mut received)
                .expect("echo bytes delivered");
            assert_eq!(&received, b"!000000000001~!000000000002~!000000000003~");
            handle.shutdown();
            assert_eq!(peer.read(&mut [0]).expect("actor closes"), 0);
        });
        if records.is_empty() {
            return;
        }
        for identity in [1, 2, 3] {
            let fragments: Vec<_> = records
                .iter()
                .filter(|record| {
                    record["stage"] == "input.actor_fragment" && record["id"] == identity
                })
                .collect();
            assert_eq!(fragments.len(), if identity == 1 { 2 } else { 1 });
            for fragment in fragments {
                let part = fragment["value"].as_u64().expect("part id");
                let boundary = |stage| {
                    records
                        .iter()
                        .find(|record| {
                            record["stage"] == stage
                                && record["scope"] == fragment["scope"]
                                && record["id"] == part
                        })
                        .expect("each accepted fragment has a boundary")["ns"]
                        .as_u64()
                        .expect("timestamp")
                };
                assert!(boundary("input.actor_enqueue") <= boundary("input.actor_claim"));
                assert!(boundary("input.actor_claim") <= boundary("input.pty_pending"));
                assert!(boundary("input.pty_pending") <= boundary("input.pty_write_attempt"));
                assert!(boundary("input.pty_write_attempt") <= boundary("input.pty_part_complete"));
            }
        }
    }

    #[cfg(feature = "latency-prof")]
    #[test]
    fn actor_trace_separates_backpressure_delayed_enter_and_queue_wait() {
        const NAME: &str = "pty::actor::unix::tests::actor_trace_separates_backpressure_delayed_enter_and_queue_wait";
        let records = trace_scenario(NAME, || {
            let (handle, mut peer, read_rx) = actor_with_socket_pair(false);
            let mut text = vec![b'x'; 1024 * 1024];
            text.extend_from_slice(b"!00000000000");
            let text_len = text.len();
            let completion = handle
                .queue_user_input_submission(
                    Bytes::from(text),
                    Bytes::from_static(b"1~"),
                    Duration::from_millis(40),
                )
                .expect("submission accepted");
            handle
                .try_write_user_input(Bytes::from_static(b"!000000000002~"))
                .expect("echo queues behind Enter");
            peer.write_all(b"readiness")
                .expect("peer writes under backpressure");
            assert_eq!(
                read_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("actor reads under backpressure"),
                Bytes::from_static(b"readiness")
            );
            let mut received = vec![0; text_len + 2 + 14];
            peer.read_exact(&mut received)
                .expect("ordered input delivered");
            assert!(received[..text_len - 12].iter().all(|byte| *byte == b'x'));
            assert_eq!(&received[text_len - 12..], b"!000000000001~!000000000002~");
            completion
                .recv_timeout(Duration::from_secs(1))
                .expect("reply")
                .expect("submission completes");
            handle
                .begin_handoff(Duration::from_secs(1))
                .expect("handoff drains writes");
            assert!(matches!(
                handle.try_write_user_input(Bytes::from_static(b"!000000000003~")),
                Err(mpsc::error::TrySendError::Closed(_))
            ));
            handle.rollback_handoff().expect("rollback resumes");
            handle
                .try_write_user_input(Bytes::from_static(b"!000000000004~"))
                .expect("post rollback echo accepted");
            let mut after = [0; 14];
            peer.read_exact(&mut after).expect("post rollback bytes");
            assert_eq!(&after, b"!000000000004~");
            handle.shutdown();
            assert_eq!(peer.read(&mut [0]).expect("actor closes"), 0);
        });
        if records.is_empty() {
            return;
        }
        let parts: Vec<_> = records
            .iter()
            .filter(|record| record["stage"] == "input.actor_fragment" && record["id"] == 1)
            .collect();
        assert_eq!(parts.len(), 2);
        let parent = |part: &serde_json::Value| {
            records
                .iter()
                .find(|record| {
                    record["stage"] == "input.actor_part"
                        && record["scope"] == part["scope"]
                        && record["id"] == part["value"]
                })
                .expect("parent link")["value"]
                .as_u64()
                .expect("command id")
        };
        assert_eq!(parent(parts[0]), parent(parts[1]));
        let time = |stage: &str, id: u64| {
            records
                .iter()
                .find(|record| record["stage"] == stage && record["id"] == id)
                .expect("boundary")["ns"]
                .as_u64()
                .expect("timestamp")
        };
        let text = parts[0]["value"].as_u64().expect("text part");
        let enter = parts[1]["value"].as_u64().expect("Enter part");
        assert!(time("input.pty_write_attempt", text) < time("input.pty_part_complete", text));
        assert!(time("input.pty_part_complete", text) < time("input.pty_pending", enter));
        let following = records
            .iter()
            .find(|record| record["stage"] == "input.actor_fragment" && record["id"] == 2)
            .expect("following echo");
        assert!(
            time("input.pty_part_complete", enter) <= time("input.actor_claim", parent(following))
        );
        assert!(!records
            .iter()
            .any(|record| record["stage"] == "input.actor_fragment" && record["id"] == 3));
    }

    #[cfg(feature = "latency-prof")]
    #[test]
    fn actor_trace_does_not_claim_rejected_or_closed_input() {
        const NAME: &str =
            "pty::actor::unix::tests::actor_trace_does_not_claim_rejected_or_closed_input";
        let records = trace_scenario(NAME, || {
            let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
            let active = handle
                .queue_user_input_submission(
                    Bytes::from_static(b"waiting"),
                    Bytes::from_static(b"\r"),
                    Duration::from_secs(10),
                )
                .expect("active submission accepted");
            let mut text = [0; 7];
            peer.read_exact(&mut text)
                .expect("actor entered submission delay");
            assert_eq!(&text, b"waiting");
            for index in 0..ACTOR_COMMAND_BUFFER {
                let bytes = if index == 0 {
                    Bytes::from_static(b"!000000000001~")
                } else {
                    Bytes::from_static(b"queued")
                };
                handle
                    .try_write_user_input(bytes)
                    .expect("bounded queue accepts capacity");
            }
            let rejected = Bytes::from_static(b"!000000000002~");
            assert!(
                matches!(handle.try_write_user_input(rejected.clone()), Err(mpsc::error::TrySendError::Full(returned)) if returned == rejected)
            );
            assert_eq!(
                handle
                    .queue_user_input_submission(rejected, Bytes::new(), Duration::ZERO)
                    .expect_err("full submission rejected")
                    .kind(),
                std::io::ErrorKind::WouldBlock
            );
            handle.shutdown();
            active
                .recv_timeout(Duration::from_secs(1))
                .expect("closed reply")
                .expect_err("active submission cancelled");
            assert_eq!(peer.read(&mut [0]).expect("actor closes"), 0);
            assert!(matches!(
                handle.try_write_user_input(Bytes::from_static(b"!000000000003~")),
                Err(mpsc::error::TrySendError::Closed(_))
            ));
        });
        if records.is_empty() {
            return;
        }
        assert!(!records
            .iter()
            .any(|record| record["stage"] == "input.actor_fragment"
                && (record["id"] == 2 || record["id"] == 3)));
        let fragment = records
            .iter()
            .find(|record| record["stage"] == "input.actor_fragment" && record["id"] == 1)
            .expect("queued echo trace");
        let parent = records
            .iter()
            .find(|record| {
                record["stage"] == "input.actor_part"
                    && record["scope"] == fragment["scope"]
                    && record["id"] == fragment["value"]
            })
            .expect("parent link");
        assert!(records
            .iter()
            .any(|record| record["stage"] == "input.actor_enqueue"
                && record["scope"] == parent["scope"]
                && record["id"] == parent["value"]));
        assert!(records
            .iter()
            .any(|record| record["stage"] == "input.actor_discard"
                && record["scope"] == parent["scope"]
                && record["id"] == parent["value"]));
        assert!(!records
            .iter()
            .any(|record| record["stage"] == "input.actor_claim"
                && record["scope"] == parent["scope"]
                && record["id"] == parent["value"]));
        assert!(!records
            .iter()
            .any(|record| record["stage"] == "input.pty_part_complete"
                && record["scope"] == fragment["scope"]
                && record["id"] == fragment["value"]));
    }

    #[test]
    fn actor_delays_enter_from_completed_prompt_write() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let text = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
        let text_len = text.len();
        let delay = Duration::from_millis(200);
        let reader = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let mut received = vec![0; text_len];
            peer.read_exact(&mut received)
                .expect("peer receives prompt");
            let prompt_completed = Instant::now();
            let mut enter = [0; 1];
            peer.read_exact(&mut enter).expect("peer receives enter");
            let enter_received = Instant::now();
            let mut user = [0; 4];
            peer.read_exact(&mut user)
                .expect("peer receives queued input");
            (prompt_completed, enter_received, enter, user)
        });

        let completion = handle
            .queue_user_input_submission(text, Bytes::from_static(b"\r"), delay)
            .expect("submission queues");
        handle
            .try_write_user_input(Bytes::from_static(b"user"))
            .expect("ordinary input queues behind submission");
        completion
            .recv()
            .expect("actor reports submission")
            .expect("submission completes");
        let (prompt_completed, enter_received, enter, user) = reader.join().expect("reader joins");

        assert_eq!(enter, *b"\r");
        assert_eq!(user, *b"user");
        assert!(enter_received.duration_since(prompt_completed) >= delay / 2);

        let err = match handle.queue_user_input_submission(
            Bytes::from_static(b"prompt"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        ) {
            Ok(completion) => completion
                .recv()
                .expect("actor reports submission")
                .expect_err("closed PTY rejects submission"),
            Err(err) => err,
        };

        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::WriteZero
        ));
    }

    #[test]
    fn actor_completes_empty_submission_parts() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let completion = handle
            .queue_user_input_submission(Bytes::new(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("empty prompt submission queues");
        let mut enter = [0; 1];
        peer.read_exact(&mut enter)
            .expect("peer receives enter for empty prompt");
        assert_eq!(enter, *b"\r");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty prompt submission")
            .expect("empty prompt submission completes");

        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::new(),
                Duration::from_millis(40),
            )
            .expect("empty enter submission queues");
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_millis(250)));
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("peer receives prompt before empty enter");
        assert_eq!(&prompt, b"prompt");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty enter submission")
            .expect("empty enter submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff resumes without an idle poll after submission");
        handle.shutdown();
    }

    #[test]
    fn actor_reports_peer_closure_during_submission_delay() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("submission queues");
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        drop(peer);

        let err = completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports peer closure")
            .expect_err("peer closure fails the active submission");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_fails_buffered_submissions_on_exit() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let active = handle
            .queue_user_input_submission(
                Bytes::from_static(b"first"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("first submission queues");
        let mut prompt = [0; 5];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        let buffered = handle
            .queue_user_input_submission(
                Bytes::from_static(b"second"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("second submission queues");

        drop(peer);
        let active_err = active
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports active submission")
            .expect_err("peer closure fails active submission");
        let buffered_err = buffered
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports buffered submission")
            .expect_err("peer closure fails buffered submission");

        assert_eq!(active_err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(buffered_err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_rejects_submission_after_io_loop_exits() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle_slot = Arc::new(Mutex::new(None::<PtyIoActorHandle>));
        let (attempt_tx, attempt_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Some(Box::new({
                let handle_slot = Arc::clone(&handle_slot);
                move || {
                    let handle = handle_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .expect("actor handle installed")
                        .clone();
                    let attempt = handle.queue_user_input_submission(
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                    );
                    attempt_tx.send(attempt).expect("attempt receiver alive");
                }
            })),
        };
        let handle = PtyIoActor::spawn(config).expect("actor spawn");
        *handle_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);

        drop(peer);
        let err = match attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader exit callback attempts submission")
        {
            Ok(completion) => completion
                .recv_timeout(Duration::from_secs(1))
                .expect("actor reports submission")
                .expect_err("closed actor rejects submission"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_reads_output_while_input_is_backpressured() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        })
        .expect("actor spawn");

        let marker = Bytes::from_static(b"queued-input");
        let completion = handle
            .queue_user_input_submission(marker.clone(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("submission accepted");

        const OUTPUT_LEN: usize = 128 * 1024;
        let mut peer_writer = peer.try_clone().expect("clone peer writer");
        let output_writer = std::thread::spawn(move || {
            peer_writer
                .write_all(&vec![0xBB; OUTPUT_LEN])
                .expect("peer writes sustained output");
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut output_len = 0;
        while output_len < OUTPUT_LEN {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "actor did not keep reading blocked peer output"
            );
            let output = read_rx
                .recv_timeout(remaining)
                .expect("actor keeps reading while input remains blocked");
            assert!(output.iter().all(|byte| *byte == 0xBB));
            output_len += output.len();
        }
        assert_eq!(output_len, OUTPUT_LEN);
        output_writer.join().expect("output writer joins");

        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));

        let mut received_input = vec![0; prefilled + marker.len() + 1];
        peer.read_exact(&mut received_input)
            .expect("peer receives prefill and queued input");
        assert!(received_input[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(
            &received_input[prefilled..prefilled + marker.len()],
            marker.as_ref()
        );
        assert_eq!(received_input.last(), Some(&b'\r'));
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff waits for submission");
        handle.shutdown();
    }

    #[test]
    fn poll_ignores_pty_hup_without_pty_interest() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        drop(peer);
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");

        let readiness = fd::poll_pty_and_wake(
            actor_socket.as_raw_fd(),
            wake_pipe.read_fd.as_raw_fd(),
            false,
            false,
            10,
        )
        .expect("poll succeeds");

        assert!(!readiness.pty_read_ready);
        assert!(!readiness.pty_write_ready);
        assert!(!readiness.wake_ready);
    }

    #[test]
    fn actor_delivers_fd_reads_to_callback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        peer.write_all(b"from-peer").expect("peer write");

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor read callback");
        assert_eq!(read, Bytes::from_static(b"from-peer"));
        handle.shutdown();
    }

    #[test]
    fn actor_delivers_only_new_bytes_across_alternating_read_lengths() {
        let (mut runner, mut peer) = actor_runner_for_unit_test();
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (read_tx, read_rx) = std_mpsc::channel();
        runner.on_read = Box::new(move |bytes| {
            read_tx
                .send(Bytes::copy_from_slice(bytes))
                .expect("read receiver alive");
            PtyReadResult {
                terminal_responses: vec![Bytes::copy_from_slice(bytes)],
            }
        });

        for payload in [
            vec![0xA5; 8192],
            b"x".to_vec(),
            b"short".to_vec(),
            vec![0x5A; 8191],
            b"end".to_vec(),
        ] {
            peer.write_all(&payload).expect("peer output");
            let mut delivered = Vec::new();
            while delivered.len() < payload.len() {
                assert!(runner.read_once(), "actor stays readable");
                delivered.extend(read_rx.try_recv().expect("callback receives pending bytes"));
                while !runner.pending_writes.is_empty() {
                    runner
                        .flush_pending_writes_once()
                        .expect("terminal response writes");
                }
            }
            assert_eq!(delivered, payload, "callback receives exact output");
            let mut response = vec![0; payload.len()];
            peer.read_exact(&mut response).expect("terminal response");
            assert_eq!(response, payload, "terminal responses preserve byte order");

            assert!(runner.read_once(), "WouldBlock keeps actor alive");
            assert!(matches!(
                read_rx.try_recv(),
                Err(std_mpsc::TryRecvError::Empty)
            ));
        }

        peer.shutdown(std::net::Shutdown::Write).expect("peer EOF");
        assert!(!runner.read_once(), "EOF terminates reading");
        assert!(matches!(
            read_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn actor_stops_after_fatal_read_error_without_delivering_old_bytes() {
        let (mut runner, mut peer) = actor_runner_for_unit_test();
        let (read_tx, read_rx) = std_mpsc::channel();
        runner.on_read = Box::new(move |bytes| {
            read_tx
                .send(Bytes::copy_from_slice(bytes))
                .expect("read receiver alive");
            PtyReadResult::empty()
        });
        peer.write_all(b"previous-output").expect("peer output");
        assert!(runner.read_once());
        assert_eq!(
            read_rx.try_recv().expect("callback output"),
            b"previous-output"[..]
        );

        runner.file = Arc::new(std::fs::File::open(".").expect("directory descriptor"));
        assert!(
            !runner.read_once(),
            "reading directory is a fatal I/O error"
        );
        assert!(matches!(
            read_rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn handoff_drain_delivers_short_output_after_full_normal_read() {
        let (mut runner, mut peer) = actor_runner_for_unit_test();
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (read_tx, read_rx) = std_mpsc::channel();
        runner.on_read = Box::new(move |bytes| {
            read_tx
                .send(Bytes::copy_from_slice(bytes))
                .expect("read receiver alive");
            PtyReadResult::empty()
        });
        peer.write_all(&[0xA5; 8192]).expect("normal output");
        let mut normal = Vec::new();
        while normal.len() < 8192 {
            assert!(runner.read_once());
            normal.extend(read_rx.try_recv().expect("normal callback"));
        }
        assert_eq!(normal, vec![0xA5; 8192]);

        let mut prefilled = 0;
        loop {
            match runner.file.as_ref().write(&[0xBB; 8192]) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0);
        runner.enqueue_write(Bytes::from_static(b"drained"));
        let handoff = std::thread::spawn(move || {
            runner.begin_handoff().expect("handoff drains writes");
            runner
        });

        for payload in [b"x".as_slice(), b"short".as_slice()] {
            peer.write_all(payload)
                .expect("output during handoff drain");
            let mut delivered = Vec::new();
            while delivered.len() < payload.len() {
                delivered.extend(
                    read_rx
                        .recv_timeout(Duration::from_secs(1))
                        .expect("handoff drains output while input is blocked"),
                );
            }
            assert_eq!(delivered, payload);
        }

        let mut input = vec![0; prefilled + 7];
        peer.read_exact(&mut input)
            .expect("peer drains queued input");
        assert!(input[..prefilled].iter().all(|byte| *byte == 0xBB));
        assert_eq!(&input[prefilled..], b"drained");
        let runner = handoff.join().expect("handoff joins");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    #[test]
    fn begin_handoff_stops_reads_and_rejects_user_writes_until_rollback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let err = handle
            .begin_handoff(Duration::from_secs(1))
            .expect_err("concurrent handoff rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        peer.write_all(b"held").expect("peer write during quiesce");
        assert!(
            read_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "actor must not read while quiesced"
        );

        handle.rollback_handoff().expect("rollback resumes actor");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reads held bytes after rollback");
        assert_eq!(read, Bytes::from_static(b"held"));

        handle
            .try_write_user_input(Bytes::from_static(b"after"))
            .expect("write accepted after rollback");
        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives after");
        assert_eq!(&buf, b"after");
        handle.shutdown();
    }

    #[test]
    fn duplicate_for_handoff_requires_quiesced_actor() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        assert!(handle.duplicate_for_handoff().is_err());
        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let duplicate = handle
            .duplicate_for_handoff()
            .expect("handoff duplicate created");
        assert!(duplicate >= 0);
        unsafe {
            libc::close(duplicate);
        }
        handle.rollback_handoff().expect("rollback resumes actor");

        peer.write_all(b"still-live").expect("peer write");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor still reads after duplicate closes");
        assert_eq!(read, Bytes::from_static(b"still-live"));
        handle.shutdown();
    }

    #[test]
    fn resize_and_nudge_keep_latest_request_when_command_queue_is_full() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(b"fill")).into())
            .expect("fill command queue");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate {
                accepting: true,
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
            })),
            controls: Arc::clone(&controls),
            response_order: Arc::new(Mutex::new(())),
            foreground_observer: PtyForegroundObserver {
                master: Weak::new(),
            },
        };

        handle.resize(20, 80, 8, 16, vec![Bytes::from_static(b"old")]);
        handle.resize(40, 120, 9, 18, vec![Bytes::from_static(b"new")]);
        handle.nudge_child_redraw_after_handoff(41, 121, 10, 20);
        handle.write_terminal_response(|| Some(Bytes::from_static(b"response")));

        let controls = controls.lock().expect("controls lock");
        assert_eq!(
            controls.resize,
            Some(PtyResizeRequest {
                resize: PtyResize {
                    rows: 40,
                    cols: 120,
                    cell_width_px: 9,
                    cell_height_px: 18,
                },
                terminal_responses: vec![Bytes::from_static(b"new")],
            })
        );
        assert_eq!(
            controls.nudge,
            Some(PtyResize {
                rows: 41,
                cols: 121,
                cell_width_px: 10,
                cell_height_px: 20,
            })
        );
        assert_eq!(
            controls.terminal_responses,
            vec![Bytes::from_static(b"response")]
        );
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let runner = PtyIoActorRunner {
            diagnostic_input: crate::latency_prof::InputStimuli::default(),
            pane_id: 1,
            file: Arc::new(std::fs::File::from(owned)),
            read_buffer: [0; ACTOR_READ_BUFFER_SIZE],
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            on_read: Box::new(move |_| PtyReadResult {
                terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                    Bytes::from_static(b"query-light")
                } else {
                    Bytes::from_static(b"query-dark")
                }],
            }),
            on_reader_exit: None,
            poll_observer: None,
        };
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate {
                accepting: true,
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
            })),
            controls,
            response_order,
            foreground_observer: PtyForegroundObserver {
                master: Arc::downgrade(&runner.file),
            },
        };
        let (changed_tx, changed_rx) = std_mpsc::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();

        let appearance = std::thread::spawn(move || {
            handle.write_terminal_response(|| {
                light.store(true, Ordering::Release);
                changed_tx.send(()).expect("notify appearance change");
                continue_rx.recv().expect("continue appearance report");
                Some(Bytes::from_static(b"live-light"))
            });
        });
        changed_rx.recv().expect("appearance changed");
        peer.write_all(b"query").expect("write query");
        let reader = std::thread::spawn(move || {
            let mut runner = runner;
            assert!(runner.read_once());
            runner
        });
        continue_tx.send(()).expect("release appearance report");
        appearance.join().expect("appearance thread joins");
        let runner = reader.join().expect("reader thread joins");

        assert_eq!(
            runner.pending_writes,
            VecDeque::from([
                PendingWrite {
                    bytes: Bytes::from_static(b"live-light"),
                    boundary: None,
                    #[cfg(feature = "latency-prof")]
                    trace: Default::default(),
                    #[cfg(feature = "latency-prof")]
                    attempted: false,
                },
                PendingWrite {
                    bytes: Bytes::from_static(b"query-light"),
                    boundary: None,
                    #[cfg(feature = "latency-prof")]
                    trace: Default::default(),
                    #[cfg(feature = "latency-prof")]
                    attempted: false,
                },
            ])
        );
    }

    #[test]
    fn resize_writes_terminal_responses_after_applying_resize() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let response = Bytes::from_static(b"\x1B[48;40;100;720;900t");

        handle.resize(40, 100, 9, 18, vec![response.clone()]);

        let mut buf = vec![0; response.len()];
        peer.read_exact(&mut buf)
            .expect("peer receives resize response");
        assert_eq!(Bytes::from(buf), response);
        handle.shutdown();
    }

    #[test]
    fn handoff_control_is_not_blocked_by_full_data_queue() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(b"fill")).into())
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate {
                accepting: true,
                #[cfg(feature = "latency-prof")]
                trace: Default::default(),
            })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            foreground_observer: PtyForegroundObserver {
                master: Weak::new(),
            },
        };

        let handoff = std::thread::spawn(move || handle.begin_handoff(Duration::from_secs(1)));
        match control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff control command")
        {
            PtyIoControlCommand::BeginHandoff(reply) => {
                reply.send(Ok(())).expect("handoff waiter alive");
            }
            _ => panic!("expected begin handoff command"),
        }

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff succeeds despite full data queue");
    }

    #[test]
    fn begin_handoff_drains_user_writes_already_in_command_queue() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(
                PtyIoDataCommand::WriteUserInput(Bytes::from_static(b"queued-before-ack")).into(),
            )
            .expect("queued write");
        let mut runner = PtyIoActorRunner {
            diagnostic_input: crate::latency_prof::InputStimuli::default(),
            pane_id: 1,
            file: Arc::new(std::fs::File::from(unsafe {
                OwnedFd::from_raw_fd(actor_socket.into_raw_fd())
            })),
            read_buffer: [0; ACTOR_READ_BUFFER_SIZE],
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: fd::create_wake_pipe().expect("wake pipe").read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };

        runner.begin_handoff().expect("handoff drains queued write");

        let mut buf = [0u8; 17];
        peer.read_exact(&mut buf)
            .expect("queued write reaches peer before quiesce ack");
        assert_eq!(&buf, b"queued-before-ack");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    #[test]
    fn release_after_commit_prevents_further_io() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle.release_after_commit().expect("actor released");
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        let _ = peer.write_all(b"ignored");
        assert!(read_rx.recv_timeout(Duration::from_millis(150)).is_err());
    }
}
