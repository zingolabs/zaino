//! Shared polling helpers for chain_index tests.
//!
//! Tests often need to wait for an async background task (sync, migration,
//! mempool refresh) to reach an observable state. Prefer `poll_until` over a
//! fixed `tokio::time::sleep(...)` so the test pays only the time actually
//! needed, with a bounded worst case.

use std::{future::Future, time::Duration};
use tokio::time::{sleep, Instant};

/// Calls `cond` every `interval` until it returns `Some(T)` or the total time
/// spent reaches `budget`. Returns the value from the successful probe, or
/// panics with `what` in the message on timeout.
pub(super) async fn poll_until<F, Fut, T>(
    what: &str,
    budget: Duration,
    interval: Duration,
    mut cond: F,
) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        if let Some(v) = cond().await {
            return v;
        }
        if Instant::now() >= deadline {
            panic!("poll_until timed out after {budget:?} waiting for: {what}");
        }
        sleep(interval).await;
    }
}
