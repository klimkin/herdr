#[cfg(feature = "latency-prof")]
use std::collections::HashMap;

/// Captured beside stable source snapshots; never enters a published codec.
#[derive(Default)]
pub(crate) struct PaneSources {
    #[cfg(feature = "latency-prof")]
    instances: HashMap<u64, u64>,
}

pub(crate) fn notification(
    source: u64,
    instance: u64,
    revision: u64,
    title_changed: bool,
    pty_requested: bool,
    woke: bool,
    delayed: bool,
) {
    if !super::active() {
        return;
    }
    let now = super::now();
    super::record_runtime_at(
        "render.notification_causes",
        source,
        u64::from(title_changed) | (u64::from(pty_requested) << 1),
        revision,
        now,
        instance,
    );
    if delayed {
        super::record_runtime_at(
            "render.notify_delayed",
            source,
            revision,
            instance,
            now,
            instance,
        );
    }
    let stage = if !title_changed && !pty_requested {
        "render.notify_suppressed"
    } else if woke {
        "render.notify_requested"
    } else {
        "render.notify_coalesced"
    };
    super::record_runtime_at(stage, source, revision, instance, now, instance);
}

impl PaneSources {
    pub(crate) fn insert(&mut self, pane: &str, instance: u64) {
        #[cfg(feature = "latency-prof")]
        if super::active() {
            self.instances
                .insert(super::bytes_id(pane.as_bytes()), instance);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = (pane, instance);
    }

    #[cfg(feature = "latency-prof")]
    pub(crate) fn instance(&self, pane: &str) -> u64 {
        #[cfg(feature = "latency-prof")]
        {
            self.instances
                .get(&super::bytes_id(pane.as_bytes()))
                .copied()
                .unwrap_or(0)
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            let _ = pane;
            0
        }
    }
}
