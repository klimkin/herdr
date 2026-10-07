pub(crate) struct BatchTrace {
    #[cfg(feature = "latency-prof")]
    id: u64,
    #[cfg(feature = "latency-prof")]
    class: &'static str,
    #[cfg(feature = "latency-prof")]
    count: u64,
}

impl BatchTrace {
    pub(crate) fn begin(class: &'static str, limit: usize) -> Self {
        #[cfg(feature = "latency-prof")]
        {
            let id = if super::active() {
                super::next_scope()
            } else {
                0
            };
            if id != 0 {
                super::record_at(class, id, limit as u64, 0, super::now());
            }
            Self {
                id,
                class,
                count: 0,
            }
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            let _ = (class, limit);
            Self {}
        }
    }

    pub(crate) fn handled(&mut self) {
        #[cfg(feature = "latency-prof")]
        {
            self.count += 1;
        }
    }
    pub(crate) fn add(&mut self, count: u64) {
        #[cfg(feature = "latency-prof")]
        {
            self.count += count;
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = count;
    }
}

impl Drop for BatchTrace {
    fn drop(&mut self) {
        #[cfg(feature = "latency-prof")]
        if self.id != 0 {
            let stage = match self.class {
                "batch.begin.server" => "batch.end.server",
                "batch.begin.api" => "batch.end.api",
                "batch.begin.internal" => "batch.end.internal",
                "batch.begin.api_barrier" => "batch.end.api_barrier",
                "batch.begin.scheduled" => "batch.end.scheduled",
                "batch.begin.accept" => "batch.end.accept",
                _ => "batch.end.unknown",
            };
            super::record_at(stage, self.id, self.count, 0, super::now());
        }
    }
}
