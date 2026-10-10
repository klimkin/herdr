use std::io;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::LocalListener;
use crate::server::client_transport::{self, ServerEvent};

/// Bounded admission keeps connection bursts from monopolizing runtime work.
pub(crate) const CLIENT_ACCEPT_BATCH_LIMIT: usize = 32;

/// Actual accept return codes, also available to the opt-in render profiler.
#[derive(Default)]
pub(crate) struct AcceptStats {
    pub(crate) attempted: u64,
    pub(crate) accepted: u64,
    pub(crate) would_block: u64,
    pub(crate) interrupted: u64,
    pub(crate) failed: u64,
}

impl AcceptStats {
    fn record(&self) {
        crate::render_prof::event("client.accept.drain");
        crate::render_prof::counter("client.accept.attempted", self.attempted);
        crate::render_prof::counter("client.accept.accepted", self.accepted);
        crate::render_prof::counter("client.accept.would_block", self.would_block);
        crate::render_prof::counter("client.accept.interrupted", self.interrupted);
        crate::render_prof::counter("client.accept.failed", self.failed);
    }
}

/// Accepts pending thin-client connections and starts their handshake readers.
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<AcceptStats> {
    let mut batch = crate::latency_prof::work::BatchTrace::begin(
        "batch.begin.accept",
        CLIENT_ACCEPT_BATCH_LIMIT,
    );
    let mut stats = AcceptStats::default();
    for _ in 0..CLIENT_ACCEPT_BATCH_LIMIT {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        stats.attempted += 1;
        batch.handled();
        match listener.accept() {
            Ok(stream) => {
                stats.accepted += 1;
                let client_id = *next_client_id;
                *next_client_id = next_client_id.saturating_add(1);
                #[cfg(feature = "latency-prof")]
                if crate::latency_prof::active() {
                    crate::latency_prof::record(
                        "client.accepted",
                        client_id,
                        u64::from(crate::platform::local_stream_peer_pid(&stream).unwrap_or(0)),
                    );
                }

                if let Err(err) = stream.set_nonblocking(true) {
                    crate::render_prof::event("client.accept.stream_setup_failed");
                    warn!(err = %err, "failed to set client stream nonblocking");
                    continue;
                }

                let should_quit = should_quit.clone();
                let server_event_tx = server_event_tx.clone();
                let spawned = crate::thread_spawn::spawn_named("herdr-client-conn", move || {
                    if let Err(err) = client_transport::handle_client_handshake(
                        stream,
                        client_id,
                        &server_event_tx,
                        &should_quit,
                    ) {
                        debug!(client_id, err = %err, "client handshake failed");
                    }
                });
                if let Err(err) = spawned {
                    warn!(client_id, err = %err, "failed to spawn client connection thread; dropping connection");
                }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                stats.would_block += 1;
                break;
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    stats.interrupted += 1;
                    continue;
                }
                stats.failed += 1;
                error!(err = %err, "client listener accept failed");
                break;
            }
        }
    }
    stats.record();
    Ok(stats)
}

/// Drains pending thin-client connections without starting handshakes.
///
/// During live handoff the old server must not let clients sit in the Unix
/// listener backlog waiting for a welcome frame that will never be sent.
pub(crate) fn reject_pending_client_connections(
    listener: &LocalListener,
) -> io::Result<AcceptStats> {
    reject_client_connections(listener, None)
}

/// Bound rejection during runtime turns so other ready work keeps progressing.
pub(crate) fn reject_client_connection_batch(listener: &LocalListener) -> io::Result<AcceptStats> {
    reject_client_connections(listener, Some(CLIENT_ACCEPT_BATCH_LIMIT))
}

fn reject_client_connections(
    listener: &LocalListener,
    attempt_limit: Option<usize>,
) -> io::Result<AcceptStats> {
    let mut stats = AcceptStats::default();
    while attempt_limit.is_none_or(|limit| stats.attempted < limit as u64) {
        stats.attempted += 1;
        match listener.accept() {
            Ok(_stream) => {
                stats.accepted += 1;
                crate::render_prof::event("client.accept.rejected");
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                stats.would_block += 1;
                break;
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    stats.interrupted += 1;
                    continue;
                }
                stats.failed += 1;
                error!(err = %err, "client listener reject failed");
                break;
            }
        }
    }
    stats.record();
    Ok(stats)
}
