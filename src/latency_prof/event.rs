#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EventTrace {
    #[cfg(feature = "latency-prof")]
    pub(crate) id: u64,
    #[cfg(feature = "latency-prof")]
    lane: u64,
    #[cfg(feature = "latency-prof")]
    selected_wait: u64,
}

#[cfg(feature = "latency-prof")]
thread_local! {
    static CURRENT: std::cell::Cell<EventTrace> = const { std::cell::Cell::new(EventTrace { id: 0, lane: 0, selected_wait: 0 }) };
    static OUTCOME: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SERVICE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TURN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl EventTrace {
    pub(crate) fn ingress(lane: u64, class: &'static str, bytes: usize) -> Self {
        #[cfg(feature = "latency-prof")]
        if super::active() {
            let trace = Self {
                id: super::next_scope(),
                lane,
                selected_wait: 0,
            };
            trace.record("event.ingress", 0);
            trace.record(class, bytes as u64);
            return trace;
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (lane, class, bytes);
        Self::default()
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn is_active(self) -> bool {
        #[cfg(feature = "latency-prof")]
        {
            self.id != 0
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            false
        }
    }

    pub(crate) fn record(self, stage: &'static str, value: u64) {
        #[cfg(feature = "latency-prof")]
        if self.id != 0 {
            let service = if CURRENT.with(std::cell::Cell::get).id == self.id {
                SERVICE.with(std::cell::Cell::get)
            } else {
                0
            };
            super::record_context_at(
                stage,
                self.id,
                value,
                self.lane,
                super::now(),
                super::TraceContext {
                    service,
                    ..super::TraceContext::default()
                },
            );
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (stage, value);
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn response(self, response: &str) {
        #[cfg(feature = "latency-prof")]
        if self.is_active() {
            let outcome = match serde_json::from_str::<serde_json::Value>(response) {
                Ok(value) if value.get("error").is_some() => 3,
                Ok(_) => 1,
                Err(_) => 0,
            };
            self.record("event.response_completed", outcome);
            // Only a response emitted by this synchronous handler closes its outcome.
            // Async replies carry origin metadata without changing another handler's TLS.
            if CURRENT.with(std::cell::Cell::get).id == self.id
                && SERVICE.with(std::cell::Cell::get) != 0
            {
                OUTCOME.with(|value| value.set(outcome));
            }
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = response;
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn with_selected_wait(mut self, wait: &super::wait::WaitTrace) -> Self {
        #[cfg(feature = "latency-prof")]
        {
            self.selected_wait = wait.identity();
            self.record("event.selected_wait", self.selected_wait);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = wait;
        self
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn deferred(mut self) -> Self {
        self.selected_wait = 0;
        self
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn received(self, lane: u64, remaining: usize) {
        #[cfg(feature = "latency-prof")]
        if super::active() {
            super::record_at(
                "lane.received",
                self.id,
                remaining as u64,
                lane,
                super::now(),
            );
            self.record("event.received", remaining as u64);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (lane, remaining);
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn completion_received(self, remaining: usize) {
        if super::active() {
            super::record_at("lane.received", self.id, remaining as u64, 3, super::now());
            self.record("event.completion_received", remaining as u64);
        }
    }

    pub(crate) fn handler_in(self, lane: u64) -> HandlerTrace {
        #[cfg(feature = "latency-prof")]
        if super::active() {
            super::record_at("lane.service", self.id, 0, lane, super::now());
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = lane;
        let handler = self.handler();
        self.record("handler.lane", lane);
        handler
    }

    pub(crate) fn handler(self) -> HandlerTrace {
        #[cfg(feature = "latency-prof")]
        let service = if self.is_active() {
            super::next_scope()
        } else {
            0
        };
        #[cfg(feature = "latency-prof")]
        if service != 0 {
            super::record_context_at(
                "handler.begin",
                self.id,
                0,
                self.lane,
                super::now(),
                super::TraceContext {
                    service,
                    ..super::TraceContext::default()
                },
            );
        }
        #[cfg(feature = "latency-prof")]
        let previous = CURRENT.with(|value| value.replace(self));
        #[cfg(feature = "latency-prof")]
        let previous_outcome = OUTCOME.with(|value| value.replace(1));
        #[cfg(feature = "latency-prof")]
        let previous_service = SERVICE.with(|value| value.replace(service));
        #[cfg(feature = "latency-prof")]
        self.record("handler.turn", TURN.with(std::cell::Cell::get));
        #[cfg(feature = "latency-prof")]
        if self.selected_wait != 0 {
            self.record("handler.selected_wait", self.selected_wait);
        }
        HandlerTrace {
            #[cfg(feature = "latency-prof")]
            trace: self,
            #[cfg(feature = "latency-prof")]
            previous,
            #[cfg(feature = "latency-prof")]
            service,
            #[cfg(feature = "latency-prof")]
            previous_outcome,
            #[cfg(feature = "latency-prof")]
            previous_service,
        }
    }
}

pub(crate) struct HandlerTrace {
    #[cfg(feature = "latency-prof")]
    trace: EventTrace,
    #[cfg(feature = "latency-prof")]
    previous: EventTrace,
    #[cfg(feature = "latency-prof")]
    service: u64,
    #[cfg(feature = "latency-prof")]
    previous_outcome: u64,
    #[cfg(feature = "latency-prof")]
    previous_service: u64,
}

impl Drop for HandlerTrace {
    fn drop(&mut self) {
        #[cfg(feature = "latency-prof")]
        if self.service != 0 {
            super::record_context_at(
                "handler.end",
                self.trace.id,
                OUTCOME.with(std::cell::Cell::get),
                self.trace.lane,
                super::now(),
                super::TraceContext {
                    service: self.service,
                    ..super::TraceContext::default()
                },
            );
        }
        #[cfg(feature = "latency-prof")]
        CURRENT.with(|value| value.set(self.previous));
        #[cfg(feature = "latency-prof")]
        OUTCOME.with(|value| value.set(self.previous_outcome));
        #[cfg(feature = "latency-prof")]
        SERVICE.with(|value| value.set(self.previous_service));
    }
}

pub(crate) fn outcome(value: u64) {
    #[cfg(feature = "latency-prof")]
    OUTCOME.with(|outcome| outcome.set(value));
    #[cfg(not(feature = "latency-prof"))]
    let _ = value;
}

#[cfg(feature = "latency-prof")]
pub(crate) fn current() -> EventTrace {
    CURRENT.with(std::cell::Cell::get)
}

/// Contains one actual outer-loop turn; async work carries explicit event metadata.
pub(crate) struct TurnTrace {
    #[cfg(feature = "latency-prof")]
    previous: u64,
}

impl TurnTrace {
    pub(crate) fn begin() -> Self {
        #[cfg(feature = "latency-prof")]
        {
            let id = if super::active() {
                super::next_scope()
            } else {
                0
            };
            let previous = TURN.with(|value| value.replace(id));
            if id != 0 {
                super::record("loop.turn", id, 0);
            }
            Self { previous }
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            Self {}
        }
    }
}

impl Drop for TurnTrace {
    fn drop(&mut self) {
        #[cfg(feature = "latency-prof")]
        TURN.with(|value| value.set(self.previous));
    }
}

#[cfg(feature = "latency-prof")]
pub(crate) fn link_actor_fragment(identity: u64, event: u64) {
    if event != 0 {
        super::record_at("input.event_fragment", identity, event, 1, super::now());
    }
}
