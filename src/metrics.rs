//! Lock-free metrics. Snapshot via [`Antibot::metrics`](crate::Antibot::metrics).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct MetricsInner {
    pub solves_attempted: AtomicU64,
    pub solves_succeeded: AtomicU64,
    pub solves_failed: AtomicU64,
    pub cache_hits: AtomicU64,
    pub coalesced_waits: AtomicU64,
    pub retries: AtomicU64,
    pub total_solve_time_ms: AtomicU64,
    pub container_restarts: AtomicU64,
}

/// Cloneable handle to the metrics ring. Cheap to clone; updates are atomic.
#[derive(Clone, Default)]
pub(crate) struct Metrics {
    inner: Arc<MetricsInner>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_attempt(&self) {
        self.inner.solves_attempted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_success(&self, elapsed_ms: u64) {
        self.inner.solves_succeeded.fetch_add(1, Ordering::Relaxed);
        self.inner
            .total_solve_time_ms
            .fetch_add(elapsed_ms, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.inner.solves_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_hit(&self) {
        self.inner.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_coalesced_wait(&self) {
        self.inner.coalesced_waits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_retry(&self) {
        self.inner.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_container_restart(&self) {
        self.inner.container_restarts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let attempted = self.inner.solves_attempted.load(Ordering::Relaxed);
        let succeeded = self.inner.solves_succeeded.load(Ordering::Relaxed);
        let failed = self.inner.solves_failed.load(Ordering::Relaxed);
        let cache_hits = self.inner.cache_hits.load(Ordering::Relaxed);
        let coalesced_waits = self.inner.coalesced_waits.load(Ordering::Relaxed);
        let retries = self.inner.retries.load(Ordering::Relaxed);
        let total_solve_time_ms = self.inner.total_solve_time_ms.load(Ordering::Relaxed);
        let container_restarts = self.inner.container_restarts.load(Ordering::Relaxed);

        let success_rate = if attempted == 0 {
            0.0
        } else {
            succeeded as f64 / attempted as f64
        };

        let avg_solve_time_ms = if succeeded == 0 {
            0.0
        } else {
            total_solve_time_ms as f64 / succeeded as f64
        };

        MetricsSnapshot {
            solves_attempted: attempted,
            solves_succeeded: succeeded,
            solves_failed: failed,
            cache_hits,
            coalesced_waits,
            retries,
            container_restarts,
            success_rate,
            avg_solve_time_ms,
        }
    }
}

/// Read-only point-in-time view of the client's metrics.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub solves_attempted: u64,
    pub solves_succeeded: u64,
    pub solves_failed: u64,
    pub cache_hits: u64,
    pub coalesced_waits: u64,
    pub retries: u64,
    pub container_restarts: u64,
    pub success_rate: f64,
    pub avg_solve_time_ms: f64,
}
