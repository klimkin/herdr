use std::time::Instant;

/// Server-only provenance; none of these fields enters a wire codec.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TraceContext {
    #[cfg(feature = "latency-prof")]
    pub(crate) presentation: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) attempt: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) serialization: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) runtime_instance: u64,
    #[cfg(feature = "latency-prof")]
    pub(crate) service: u64,
}

pub(crate) struct PresentationTrace {
    context: TraceContext,
}

impl PresentationTrace {
    pub(crate) fn selected(decision: Instant, deadline: Instant) -> Self {
        let context = TraceContext::default();
        #[cfg(feature = "latency-prof")]
        let mut context = context;
        #[cfg(feature = "latency-prof")]
        if super::enabled::active() {
            context.presentation = super::next_scope();
            let anchor = Instant::now();
            let anchor_ns = super::now();
            let convert = |time: Instant| {
                if time >= anchor {
                    anchor_ns.saturating_add(time.duration_since(anchor).as_nanos() as u64)
                } else {
                    anchor_ns.saturating_sub(anchor.duration_since(time).as_nanos() as u64)
                }
            };
            super::record_context_at(
                "presentation.selected_deadline",
                context.presentation,
                convert(deadline),
                0,
                anchor_ns,
                context,
            );
            super::record_context_at(
                "presentation.decision",
                context.presentation,
                convert(decision),
                0,
                anchor_ns,
                context,
            );
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (decision, deadline);
        Self { context }
    }

    pub(crate) fn context(&self) -> TraceContext {
        self.context
    }
}

/// Immutable attempt context survives every early return; drop closes the span.
pub(crate) struct AttemptTrace {
    context: TraceContext,
}

impl AttemptTrace {
    pub(crate) fn begin(context: TraceContext, retained: bool, recipients: usize) -> Self {
        #[cfg(feature = "latency-prof")]
        let mut context = context;
        #[cfg(feature = "latency-prof")]
        if context.presentation != 0 {
            context.attempt = super::next_scope();
            super::record_context_at(
                "server.frame_start",
                context.attempt,
                if retained { 1 } else { 2 },
                recipients as u64,
                super::now(),
                context,
            );
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (retained, recipients);
        Self { context }
    }

    pub(crate) fn context(&self) -> TraceContext {
        self.context
    }

    pub(crate) fn outcome(&self, stage: &'static str) {
        #[cfg(feature = "latency-prof")]
        if self.context.attempt != 0 {
            super::record_context_at(
                stage,
                self.context.attempt,
                0,
                0,
                super::now(),
                self.context,
            );
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = stage;
    }
}

impl Drop for AttemptTrace {
    fn drop(&mut self) {
        self.outcome("server.frame_end");
    }
}

/// One explicitly serialized frame at a byte position within an enqueue batch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SerializedFrame {
    #[cfg(feature = "latency-prof")]
    offset: u64,
    #[cfg(feature = "latency-prof")]
    len: u64,
    #[cfg(feature = "latency-prof")]
    fingerprint: u64,
    #[cfg(feature = "latency-prof")]
    context: TraceContext,
}

pub(crate) fn serialized(
    msg: &crate::protocol::ServerMessage,
    framed: &[u8],
    context: TraceContext,
    sources: &super::runtime::PaneSources,
    prepared_panes: Option<&[crate::protocol::PaneSurfacePane]>,
) -> Vec<SerializedFrame> {
    #[cfg(feature = "latency-prof")]
    let mut context = context;
    #[cfg(feature = "latency-prof")]
    if context.attempt != 0 {
        context.serialization = super::next_scope();
        let fingerprint = framed.get(4..).map(super::bytes_id).unwrap_or(0);
        let len = framed.len().saturating_sub(4) as u64;
        super::record_context_at(
            "server.serialization",
            fingerprint,
            len,
            0,
            super::now(),
            context,
        );
        let panes = prepared_panes.or(match msg {
            crate::protocol::ServerMessage::PaneSurface(surface) => Some(surface.panes.as_slice()),
            crate::protocol::ServerMessage::PaneSurfacePatch(patch) => Some(patch.panes.as_slice()),
            _ => None,
        });
        if let Some(panes) = panes {
            for pane in panes {
                let pane_context = TraceContext {
                    runtime_instance: sources.instance(&pane.pane_id),
                    ..context
                };
                super::record_context_at(
                    "surface.content",
                    super::bytes_id(pane.pane_id.as_bytes()),
                    pane.content_revision,
                    fingerprint,
                    super::now(),
                    pane_context,
                );
                super::record_context_at(
                    "surface.geometry",
                    super::bytes_id(pane.pane_id.as_bytes()),
                    u64::from(pane.inner_rect.width) << 32 | u64::from(pane.inner_rect.height),
                    fingerprint,
                    super::now(),
                    pane_context,
                );
            }
        }
        return vec![SerializedFrame {
            offset: 0,
            len,
            fingerprint,
            context,
        }];
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (msg, framed, context, sources, prepared_panes);
    Vec::new()
}

pub(crate) fn append(
    frames: &mut Vec<SerializedFrame>,
    mut extra: Vec<SerializedFrame>,
    offset: usize,
) {
    #[cfg(feature = "latency-prof")]
    for frame in &mut extra {
        frame.offset = frame.offset.saturating_add(offset as u64);
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = offset;
    frames.append(&mut extra);
}

#[cfg(feature = "latency-prof")]
pub(crate) fn bind(queued: &mut [super::QueuedFrame], serialized: &[SerializedFrame]) {
    if serialized.is_empty() {
        return;
    }
    #[cfg(feature = "latency-prof")]
    {
        let mut offset = 0;
        let valid = queued.len() == serialized.len()
            && queued.iter().zip(serialized).all(|(frame, trace)| {
                let matches = trace.offset == offset
                    && trace.len == frame.len
                    && trace.fingerprint == frame.identity.fingerprint;
                offset += frame.len + 4;
                matches
            });
        if !valid {
            super::record("server.serialization_invalid", 0, serialized.len() as u64);
            return;
        }
        for (frame, trace) in queued.iter_mut().zip(serialized) {
            frame.context = trace.context;
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (queued, serialized);
}

pub(crate) fn snapshot_links(
    snapshot: &crate::protocol::ClientShellSnapshot,
    frames: &[SerializedFrame],
) {
    #[cfg(feature = "latency-prof")]
    if let [frame] = frames {
        for workspace in &snapshot.workspaces {
            super::record_context_at(
                "snapshot.label",
                super::bytes_id(workspace.label.as_bytes()),
                snapshot.revision,
                frame.fingerprint,
                super::now(),
                frame.context,
            );
        }
    }
    #[cfg(not(feature = "latency-prof"))]
    let _ = (snapshot, frames);
}
