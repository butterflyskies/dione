use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

pub(crate) fn mark(stage: &str) {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if std::env::var_os("DIONE_BIND_DIAGNOSTICS").is_some()
        && COUNT.fetch_add(1, Ordering::Relaxed) < 256
    {
        eprintln!(
            "BIND_DIAG pid={} mono={:?} stage={stage}",
            std::process::id(),
            Instant::now()
        );
    }
}
pub(crate) fn sample(stage: &str) {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if COUNT.fetch_add(1, Ordering::Relaxed) < 12 {
        mark(stage);
    }
}
