//! Coalesce concurrent solves for the same key. When N callers race to solve
//! the same domain (or URL), only one provider call is made and the rest wait
//! on the same result.

use crate::error::AntibotError;
use crate::metrics::Metrics;
use crate::types::{Solution, SolutionSource};
use dashmap::DashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Notify;

/// Strategy for grouping in-flight solves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceKey {
    /// Coalesce all solves for the same registrable host (e.g. `walmart.com`).
    Domain,
    /// Coalesce only when the full URL matches.
    Url,
}

/// Cloneable shared result. Errors aren't `Clone`, so waiters receive a
/// stringified error wrapped in [`AntibotError::CoalescedFailure`].
type SharedResult = Result<Arc<Solution>, Arc<String>>;

type ResultHolder = Arc<sync_mutex::Mutex<Option<SharedResult>>>;

struct InflightSolve {
    notify: Arc<Notify>,
    result: ResultHolder,
}

#[derive(Clone)]
pub(crate) struct SolveCoalescer {
    inflight: Arc<DashMap<String, InflightSolve>>,
    key_strategy: CoalesceKey,
    metrics: Metrics,
}

/// Removes the leader's inflight entry and wakes waiters even if the leader's
/// future is cancelled mid-solve. Without this, a cancelled leader would leave
/// a stale entry behind and every future solve for the key would wait forever.
struct LeaderGuard {
    inflight: Arc<DashMap<String, InflightSolve>>,
    key: String,
    notify: Arc<Notify>,
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        self.inflight.remove(&self.key);
        self.notify.notify_waiters();
    }
}

impl SolveCoalescer {
    pub fn new(key_strategy: CoalesceKey, metrics: Metrics) -> Self {
        Self {
            inflight: Arc::new(DashMap::new()),
            key_strategy,
            metrics,
        }
    }

    /// Compute the coalescer key for a request.
    pub fn key_for(&self, url: &str) -> Option<String> {
        match self.key_strategy {
            CoalesceKey::Domain => crate::session_cache::extract_domain(url),
            CoalesceKey::Url => Some(url.to_string()),
        }
    }

    /// Either run `solver` (we're first) or wait for the in-flight result.
    pub async fn solve_or_wait<F, Fut>(
        &self,
        key: String,
        solver: F,
    ) -> Result<Solution, AntibotError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Solution, AntibotError>>,
    {
        let mut solver = Some(solver);
        let mut wait_recorded = false;

        loop {
            // Atomically either join an in-flight solve or claim leadership.
            // The entry guard locks the shard, so no awaits while it's held.
            let role = {
                use dashmap::mapref::entry::Entry;
                match self.inflight.entry(key.clone()) {
                    Entry::Occupied(occupied) => {
                        let slot = occupied.get();
                        Role::Waiter {
                            notify: slot.notify.clone(),
                            result: slot.result.clone(),
                        }
                    }
                    Entry::Vacant(vacant) => {
                        let notify = Arc::new(Notify::new());
                        let result: ResultHolder = Arc::new(sync_mutex::Mutex::new(None));
                        vacant.insert(InflightSolve {
                            notify: notify.clone(),
                            result: result.clone(),
                        });
                        Role::Leader { notify, result }
                    }
                }
            };

            match role {
                Role::Leader { notify, result } => {
                    let _guard = LeaderGuard {
                        inflight: self.inflight.clone(),
                        key: key.clone(),
                        notify,
                    };

                    let outcome = solver.take().expect("leader claimed twice")().await;

                    // Publish a Clone-friendly version for waiters before the
                    // guard removes the entry and notifies them.
                    let shared: SharedResult = match &outcome {
                        Ok(sol) => Ok(Arc::new(sol.clone())),
                        Err(e) => Err(Arc::new(e.to_string())),
                    };
                    *result.lock() = Some(shared);

                    return outcome;
                }
                Role::Waiter { notify, result } => {
                    // Once per call, even if a cancelled leader forces a retry.
                    if !wait_recorded {
                        self.metrics.record_coalesced_wait();
                        wait_recorded = true;
                    }

                    // Register interest BEFORE checking the result, otherwise
                    // the leader's notify_waiters() can fire in the gap and the
                    // wakeup is lost forever.
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();

                    if let Some(snapshot) = result.lock().clone() {
                        return waiter_outcome(snapshot);
                    }

                    // The leader may have finished (or been cancelled) between
                    // our entry lookup and registration; if its entry is gone,
                    // no further notification is coming.
                    let still_inflight = self
                        .inflight
                        .get(&key)
                        .map(|e| Arc::ptr_eq(&e.result, &result))
                        .unwrap_or(false);
                    if !still_inflight {
                        match result.lock().clone() {
                            Some(snapshot) => return waiter_outcome(snapshot),
                            // Leader was cancelled without a result; retry.
                            None => continue,
                        }
                    }

                    notified.await;

                    match result.lock().clone() {
                        Some(snapshot) => return waiter_outcome(snapshot),
                        // Leader was cancelled without a result; retry.
                        None => continue,
                    }
                }
            }
        }
    }
}

enum Role {
    Leader {
        notify: Arc<Notify>,
        result: ResultHolder,
    },
    Waiter {
        notify: Arc<Notify>,
        result: ResultHolder,
    },
}

fn waiter_outcome(snapshot: SharedResult) -> Result<Solution, AntibotError> {
    match snapshot {
        Ok(arc_sol) => Ok(stamp_cached((*arc_sol).clone())),
        Err(arc_msg) => Err(AntibotError::CoalescedFailure((*arc_msg).clone())),
    }
}

/// When a coalesced waiter receives a leader's solution, mark its source so the
/// caller can tell it didn't trigger the actual solve.
fn stamp_cached(mut sol: Solution) -> Solution {
    let age = sol.solved_at.elapsed().unwrap_or_default();
    sol.source = SolutionSource::Cached { age };
    sol
}

// std::sync::Mutex wrapper that ignores poisoning: guards are only held for
// cheap clone/assign operations, never across awaits.
mod sync_mutex {
    use std::sync::Mutex as StdMutex;

    pub struct Mutex<T>(StdMutex<T>);

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self(StdMutex::new(value))
        }

        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}
