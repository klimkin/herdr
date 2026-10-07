use std::time::Instant;

/// Same minimum selected for the real waiter; reasons add no scheduling policy.
#[derive(Clone, Copy, Default)]
pub(crate) struct DeadlineSelection {
    pub(crate) deadline: Option<Instant>,
    #[cfg(feature = "latency-prof")]
    reasons: u64,
}

impl DeadlineSelection {
    pub(crate) fn include(&mut self, deadline: Option<Instant>, reason: u64) {
        let Some(deadline) = deadline else {
            return;
        };
        if self.deadline.is_none_or(|current| deadline < current) {
            self.deadline = Some(deadline);
            #[cfg(feature = "latency-prof")]
            {
                self.reasons = reason;
            }
        } else if self.deadline == Some(deadline) {
            #[cfg(feature = "latency-prof")]
            {
                self.reasons |= reason;
            }
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = reason;
    }
}

pub(crate) struct WaitTrace {
    #[cfg(feature = "latency-prof")]
    id: u64,
    #[cfg(feature = "latency-prof")]
    deadline: u64,
    #[cfg(feature = "latency-prof")]
    reasons: u64,
}

impl WaitTrace {
    pub(crate) fn begin(selection: DeadlineSelection) -> Self {
        #[cfg(feature = "latency-prof")]
        {
            if !super::active() {
                return Self {
                    id: 0,
                    deadline: 0,
                    reasons: 0,
                };
            }
            let anchor = Instant::now();
            let ns = super::now();
            let deadline = selection.deadline.map_or(0, |deadline| {
                if deadline >= anchor {
                    ns.saturating_add(deadline.duration_since(anchor).as_nanos() as u64)
                } else {
                    ns.saturating_sub(anchor.duration_since(deadline).as_nanos() as u64)
                }
            });
            let trace = Self {
                id: super::next_scope(),
                deadline,
                reasons: selection.reasons,
            };
            super::record_at("loop.wait_begin", trace.id, deadline, trace.reasons, ns);
            trace
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            let _ = selection;
            Self {}
        }
    }

    pub(crate) fn returned(&self, stage: &'static str) {
        #[cfg(feature = "latency-prof")]
        if self.id != 0 {
            super::record_at(stage, self.id, self.deadline, self.reasons, super::now());
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = stage;
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn identity(&self) -> u64 {
        self.id
    }
}
