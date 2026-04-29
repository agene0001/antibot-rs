//! Coalesce concurrent solves for the same key. When N callers race to solve
//! the same domain (or URL), only one provider call is made and the rest wait
//! on the same result.

use crate::error::AntibotError;
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

struct InflightSolve {
    notify: Arc<Notify>,
    result: Arc<parking_lot_mutex::Mutex<Option<SharedResult>>>,
}

#[derive(Clone)]
pub(crate) struct SolveCoalescer {
    inflight: Arc<DashMap<String, InflightSolve>>,
    key_strategy: CoalesceKey,
}

impl SolveCoalescer {
    pub fn new(key_strategy: CoalesceKey) -> Self {
        Self {
            inflight: Arc::new(DashMap::new()),
            key_strategy,
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
        // Fast path: check if a leader is already running.
        if let Some(existing) = self.inflight.get(&key) {
            let notify = existing.notify.clone();
            let result_holder = existing.result.clone();
            drop(existing);

            notify.notified().await;

            let snapshot = result_holder.lock().clone();
            return match snapshot {
                Some(Ok(arc_sol)) => Ok(stamp_cached((*arc_sol).clone())),
                Some(Err(arc_msg)) => Err(AntibotError::CoalescedFailure((*arc_msg).clone())),
                None => Err(AntibotError::CoalescedFailure(
                    "leader vanished without producing a result".to_string(),
                )),
            };
        }

        // Slow path: insert inflight slot, then double-check we won the race.
        let notify = Arc::new(Notify::new());
        let result_holder = Arc::new(parking_lot_mutex::Mutex::new(None));

        let entry = self.inflight.entry(key.clone()).or_insert_with(|| InflightSolve {
            notify: notify.clone(),
            result: result_holder.clone(),
        });

        // If another task slipped in between the get() above and this insert,
        // fall back to waiter behavior.
        let we_are_leader = Arc::ptr_eq(&entry.notify, &notify);
        let actual_notify = entry.notify.clone();
        let actual_result = entry.result.clone();
        drop(entry);

        if !we_are_leader {
            actual_notify.notified().await;
            let snapshot = actual_result.lock().clone();
            return match snapshot {
                Some(Ok(arc_sol)) => Ok(stamp_cached((*arc_sol).clone())),
                Some(Err(arc_msg)) => Err(AntibotError::CoalescedFailure((*arc_msg).clone())),
                None => Err(AntibotError::CoalescedFailure(
                    "leader vanished without producing a result".to_string(),
                )),
            };
        }

        // We are the leader. Run the solver.
        let outcome = solver().await;

        // Publish a Clone-friendly version for waiters.
        let shared: SharedResult = match &outcome {
            Ok(sol) => Ok(Arc::new(sol.clone())),
            Err(e) => Err(Arc::new(e.to_string())),
        };
        *result_holder.lock() = Some(shared);
        self.inflight.remove(&key);
        notify.notify_waiters();

        outcome
    }
}

/// When a coalesced waiter receives a leader's solution, mark its source so the
/// caller can tell it didn't trigger the actual solve.
fn stamp_cached(mut sol: Solution) -> Solution {
    let age = sol.solved_at.elapsed().unwrap_or_default();
    sol.source = SolutionSource::Cached { age };
    sol
}

// Tiny re-export shim: tokio doesn't ship a sync Mutex outside of `parking_lot`,
// and we don't want to await across guard holds. Use std::sync::Mutex with a
// Result-friendly wrapper to avoid the poisoning surface.
mod parking_lot_mutex {
    use std::sync::Mutex as StdMutex;

    pub struct Mutex<T>(StdMutex<T>);

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self(StdMutex::new(value))
        }

        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            // A panic while a result is being published is a bug; surface it
            // via panic propagation instead of carrying poisoning into the
            // public error type.
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}
