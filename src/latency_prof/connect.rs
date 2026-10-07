//! Attach timestamps are diagnostics, separate from wire/frame connection IDs.

/// One local syscall attempt; retries have distinct identities.
pub(crate) struct ConnectTrace {
    #[cfg(feature = "latency-prof")]
    id: u64,
}

impl ConnectTrace {
    pub(crate) fn begin() -> Self {
        #[cfg(feature = "latency-prof")]
        {
            let id = if super::active() {
                super::next_scope()
            } else {
                0
            };
            if id != 0 {
                super::record("client.connect_begin", id, 0);
            }
            Self { id }
        }
        #[cfg(not(feature = "latency-prof"))]
        Self {}
    }

    pub(crate) fn complete(&self, result: &std::io::Result<crate::ipc::LocalStream>) {
        #[cfg(feature = "latency-prof")]
        if self.id != 0 {
            // Timestamp syscall return before peer lookup or diagnostic emission.
            let completed = super::now();
            let (stage, peer) = match result {
                Ok(stream) => (
                    "client.connect_complete",
                    u64::from(crate::platform::local_stream_peer_pid(stream).unwrap_or(0)),
                ),
                Err(_) => ("client.connect_failed", 0),
            };
            super::record_at(stage, self.id, peer, 0, completed);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = result;
    }
}

pub(crate) struct HandshakeTrace {
    #[cfg(feature = "latency-prof")]
    id: u64,
}

impl HandshakeTrace {
    pub(crate) fn begin() -> Self {
        #[cfg(feature = "latency-prof")]
        {
            let id = if super::active() {
                super::next_scope()
            } else {
                0
            };
            if id != 0 {
                super::record("client.hello_begin", id, 0);
            }
            Self { id }
        }
        #[cfg(not(feature = "latency-prof"))]
        Self {}
    }

    pub(crate) fn returned(&self, stage: &'static str) {
        #[cfg(feature = "latency-prof")]
        if self.id != 0 {
            super::record(stage, self.id, 0);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = stage;
    }
}
