#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

//! Low-level async/tokio building blocks, one layer **below** the component and
//! supervision model. Domain-free: no Zcash, no indexing — only concurrency
//! plumbing. Provides [`Task`], the named, panic-rendering task the component
//! layer and its consumers spawn through.

mod panic;
mod task;

pub use panic::{catch_panic, panic_message};
pub use task::{Task, TaskError, TaskName};

// The cooperative-cancellation token a [`Task`] body receives, re-exported so a
// consumer naming it (e.g. a server's run signature) need not depend on
// `tokio-util` directly. This is its owning home; the component layer re-exports
// it in turn for the traits whose signatures take it.
pub use tokio_util::sync::CancellationToken;
