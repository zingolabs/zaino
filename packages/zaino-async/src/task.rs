//! A named, supervised async task.
//!
//! Bundles the plumbing a raw `tokio::spawn` leaves to the caller: a name (for
//! status and logs), cooperative cancellation, a hard abort, and panic capture
//! on join. Generic over the task's output `T` (defaulting to `()` for the
//! fire-and-forget case), so a value-returning worker is joined for its result
//! and a unit task simply yields `()` — both through the same panic-rendered
//! [`join`](Task::join).

use core::fmt;
use core::future::Future;

use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

/// A task's name, carried on the task and its errors for status and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskName(pub &'static str);

impl fmt::Display for TaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// What can go wrong awaiting a supervised task.
///
/// Renders a `tokio` [`JoinError`] by **our** [`TaskName`], never its opaque
/// runtime task id — and keeps a panic's message, so a caller surfacing this
/// (e.g. on a health `reason`) still says *what* failed, not merely *that* a
/// task died.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The task's future panicked, with this message (a note if the panic
    /// payload was not a string).
    #[error("task '{name}' panicked: {message}")]
    Panicked {
        /// The task that panicked.
        name: TaskName,
        /// The panic message.
        message: String,
    },
    /// The task was cancelled or aborted before it finished.
    #[error("task '{name}' was cancelled")]
    Cancelled {
        /// The task that was cancelled.
        name: TaskName,
    },
}

/// A named, supervised `tokio` task producing a value of type `T` (defaulting to
/// `()` for a fire-and-forget task).
pub struct Task<T = ()> {
    name: TaskName,
    handle: JoinHandle<T>,
    cancel: CancellationToken,
}

impl<T: Send + 'static> Task<T> {
    /// Spawn `body`, handing it a [`CancellationToken`] it should poll to stop
    /// cooperatively. The task is named for status and logs. The body's output
    /// becomes the task's output, recovered by [`join`](Self::join).
    pub fn spawn<B, Fut>(name: TaskName, body: B) -> Self
    where
        B: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(body(cancel.clone()));
        Self {
            name,
            handle,
            cancel,
        }
    }

    /// The task's name.
    pub fn name(&self) -> TaskName {
        self.name
    }

    /// Ask the task to stop cooperatively at its next cancellation point.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Stop the task immediately, without giving it a chance to clean up.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Whether the task has finished — completed, panicked, or aborted.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Wait for the task to finish, yielding its output — or surfacing a panic
    /// or an abort as a typed [`TaskError`] named by *this* task, with a panic's
    /// message preserved.
    pub async fn join(self) -> Result<T, TaskError> {
        match self.handle.await {
            Ok(value) => Ok(value),
            Err(err) if err.is_panic() => Err(TaskError::Panicked {
                name: self.name,
                message: panic_message(err),
            }),
            Err(_) => Err(TaskError::Cancelled { name: self.name }),
        }
    }
}

/// Extract a panic's message from a `JoinError` known to be a panic, never
/// surfacing tokio's runtime task id. Mirrors the panic hook's payload
/// downcast (`zaino_logging`), so origin and join render the same text.
fn panic_message(err: JoinError) -> String {
    let payload = err.into_panic();
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Task, TaskError, TaskName};

    #[tokio::test]
    async fn joins_a_returned_value() {
        let task = Task::spawn(TaskName("compute"), |_cancel| async move { 6 * 7 });
        assert_eq!(task.join().await.expect("no panic"), 42);
    }

    #[tokio::test]
    async fn a_unit_task_defaults_and_joins() {
        // No turbofish needed: the `()` body infers `Task<()>`.
        let task = Task::spawn(TaskName("side-effect"), |_cancel| async move {});
        assert_eq!(task.join().await.expect("no panic"), ());
    }

    #[tokio::test]
    async fn a_panic_becomes_a_named_taskerror_with_the_message() {
        let task = Task::spawn(TaskName("doomed"), |_cancel| async move {
            panic!("boom at the seam");
            #[allow(unreachable_code)]
            42
        });
        match task.join().await {
            Err(TaskError::Panicked { name, message }) => {
                assert_eq!(name, TaskName("doomed"));
                assert_eq!(message, "boom at the seam");
            }
            other => panic!("expected a named panic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_aborted_task_is_cancelled() {
        let task = Task::spawn(TaskName("long"), |_cancel| async move {
            // Never resolves on its own; the abort ends it.
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(matches!(
            task.join().await,
            Err(TaskError::Cancelled {
                name: TaskName("long")
            })
        ));
    }
}
