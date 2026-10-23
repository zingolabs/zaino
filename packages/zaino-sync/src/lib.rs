//! The finalized path: [`FinalizedFollower`] → [`FinalizedStream<Block>`] → each index's writer
//! loop (`docs/architecture.md` §7)
//!
//! - One sender, every step finalized: each height once, ascending, never retracted
//! - Each writer: its own loop over its [`Subscription`]'s runs, its store published through an
//!   [`IndexPublisher`]
//! - An index's per-block output for another = a second sink (utxo-set → [`FeeSink`])

#![forbid(unsafe_code)]

mod fetch;
mod finalized_stream;
mod follower;
mod handle;
mod metrics;
mod offload;
mod progress;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod writer;

pub use self::metrics::METRICS;
pub(crate) use fetch::fetch_at;
pub use fetch::{fetch, Checked};
pub use finalized_stream::{FeeSink, FinalizedStream, Run, Step, Subscription, Weight};
pub use follower::{FinalizedFollower, FollowError};
pub use handle::IndexHandle;
pub use offload::{blocking, compute};
pub use progress::SyncProgress;
pub use writer::{apply, assert_dense, assert_next, assert_run, commit, held, ran, IndexPublisher};
