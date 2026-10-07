use std::sync::atomic::{AtomicU64, Ordering};

/// Counter lifetime identity; independent of capture and durable terminal IDs.
pub(crate) fn allocate() -> std::io::Result<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map_err(|_| std::io::Error::other("terminal runtime instance exhausted"))
}
