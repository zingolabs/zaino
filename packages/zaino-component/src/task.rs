//! A supervised async task — the low primitive a component runs its work on.
//!
//! Bundles the plumbing a raw `tokio::spawn` leaves to the caller: a name (for
//! status and logs), cooperative cancellation, a hard abort, and panic capture
//! on join. A component owns one or more of these; a task failing is the event
//! that flips the component's health.

use core::fmt;
use core::future::Future;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A task's name, carried on the task and its errors for status and logs.
///
/// A newtype rather than a bare `&'static str` so the name is one named concept
/// wherever it travels, not an anonymous string repeated at every site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskName(pub &'static str);

impl fmt::Display for TaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// What can go wrong awaiting a supervised task.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The task's future panicked.
    #[error("task '{name}' panicked")]
    Panicked {
        /// The task that panicked.
        name: TaskName,
    },
    /// The task was cancelled or aborted before it finished.
    #[error("task '{name}' was cancelled")]
    Cancelled {
        /// The task that was cancelled.
        name: TaskName,
    },
}

/// A named, supervised `tokio` task.
pub struct Task {
    name: TaskName,
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Task {
    /// Spawn `body`, handing it a [`CancellationToken`] it should poll to stop
    /// cooperatively. The task is named for status and logs.
    pub fn spawn<B, Fut>(name: TaskName, body: B) -> Self
    where
        B: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
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

    /// Wait for the task to finish, surfacing a panic or an abort as a typed
    /// [`TaskError`].
    pub async fn join(self) -> Result<(), TaskError> {
        match self.handle.await {
            Ok(()) => Ok(()),
            Err(err) if err.is_panic() => Err(TaskError::Panicked { name: self.name }),
            Err(_) => Err(TaskError::Cancelled { name: self.name }),
        }
    }
}
