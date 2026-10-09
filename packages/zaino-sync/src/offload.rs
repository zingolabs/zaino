//! Work that must not run on an async worker, awaited like any I/O
//!
//! - [`compute`] = CPU-bound index construction (fold, encode, project, sort) on the rayon pool,
//!   one thread per core; inside it, `rayon::par_*` spreads data-parallel work over that pool
//! - [`blocking`] = a blocking syscall (pwrite, fsync, rename) on tokio's blocking pool
//! - a panic in either resumes on the awaiting task (zainod: abort, never a half-built state)
//! - the caller's tracing span rides along (a pool thread's lines keep their component)
//!
//! ```ignore
//! let chunk = compute(move || fold(blocks)).await;
//! let store = blocking(move || store.write(chunk)).await?;
//! ```

use std::panic::{self, AssertUnwindSafe};

/// Runs `f` on the CPU pool
pub async fn compute<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, outcome) = tokio::sync::oneshot::channel();
    let span = tracing::Span::current();
    rayon::spawn(move || {
        // receiver gone = its task was cancelled: nothing left to deliver to
        let _ = done.send(panic::catch_unwind(AssertUnwindSafe(|| span.in_scope(f))));
    });
    match outcome.await.expect("compute job delivers its outcome or panics") {
        Ok(value) => value,
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Runs `f` on the blocking-I/O pool
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(f))
        .await
        .unwrap_or_else(|join| panic::resume_unwind(join.into_panic()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both hops return the job's value from off the runtime, and a job's panic resumes on the
    /// awaiting task with its own message
    #[tokio::test]
    async fn offloaded_jobs_return_their_value_and_resume_their_panic() {
        let off_runtime = || tokio::runtime::Handle::try_current().is_err();
        assert!(compute(off_runtime).await, "compute ran on a runtime thread");
        assert_eq!(compute(|| (1..=4u64).product::<u64>()).await, 24);
        assert_eq!(blocking(|| 7).await, 7);

        for hop in ["compute", "blocking"] {
            let task = tokio::spawn(async move {
                match hop {
                    "compute" => compute(|| panic!("compute blew up")).await,
                    _ => blocking(|| panic!("blocking blew up")).await,
                }
            });
            let panic = task.await.expect_err("panic resumed").into_panic();
            let payload = format!("{hop} blew up");
            assert_eq!(panic.downcast_ref::<&str>(), Some(&payload.as_str()));
        }
    }
}
