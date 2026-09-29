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

/// Writer-owned state (a store's files) that crosses to a pool for one hop and comes back
///
/// - [`get`](Self::get) = the state itself, never a cached copy of it
/// - absent inside a hop (which holds `&mut self`), or while [`lend`](Self::lend)ed to a
///   detached write until [`restore`](Self::restore)d: a read there panics, never waits
pub struct Offloaded<S>(Option<S>);

impl<S: Send + 'static> Offloaded<S> {
    pub fn new(state: S) -> Self {
        Self(Some(state))
    }

    pub fn get(&self) -> &S {
        self.0.as_ref().expect("offloaded state read mid-hop (its hop was cancelled)")
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.as_mut().expect("offloaded state read mid-hop (its hop was cancelled)")
    }

    /// The state itself, for a write that outlives this call (an [`IndexWriter::finalize`]
    /// write); back through [`restore`](Self::restore)
    ///
    /// [`IndexWriter::finalize`]: crate::IndexWriter::finalize
    pub fn lend(&mut self) -> S {
        self.0.take().expect("offloaded state already lent")
    }

    pub fn restore(&mut self, state: S) {
        assert!(self.0.replace(state).is_none(), "offloaded state restored twice");
    }

    /// `f` on the CPU pool, the state moved there and back
    pub async fn compute<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut S) -> T + Send + 'static,
    ) -> T {
        let mut state = self.0.take().expect("offloaded state already mid-hop");
        let (state, out) = compute(move || {
            let out = f(&mut state);
            (state, out)
        })
        .await;
        self.0 = Some(state);
        out
    }
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

        // state crosses, is mutated there, and is back for the next read; a lent state
        // mutated by its borrower comes back through `restore`
        let mut state = Offloaded::new(vec![1u8]);
        let mut lent = state.lend();
        lent.push(2);
        state.restore(lent);
        assert!(state.compute(move |_| off_runtime()).await, "state hop ran on a runtime thread");
        state.compute(|v| v.push(3)).await;
        state.get_mut().push(4);
        assert_eq!(state.get(), &[1, 2, 3, 4], "every hop's mutation kept, in order");

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
