//! Provisioner trait — source data acquisition.
//!
//! The provisioner owns all source access. Indexes never call the source
//! directly (except via the [`NonLocalSource`](crate::traits::NonLocalSource)
//! escape hatch). The engine tells the provisioner what data to fetch
//! (via [`SourceRequirements`](crate::descriptor::SourceRequirements)),
//! and the provisioner streams block contexts as they become ready.

use crate::descriptor::SourceRequirements;

/// Errors from provisioner operations.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The source was unreachable or returned an error.
    #[error("source error: {0}")]
    Source(String),
    /// The channel to the engine was closed.
    #[error("channel closed")]
    ChannelClosed,
}

/// The provisioner: first-class owner of all source access.
///
/// Generic — no blockchain knowledge. The `BlockContext` associated type
/// is opaque to the engine; extractors know its concrete type through
/// [`IndexDef::Context`](crate::traits::IndexDef::Context).
///
/// The provisioner streams contexts as they become ready. It may
/// internally parallelise source fetches. Backpressure propagates
/// from the engine's bounded channel.
pub trait Provisioner: Send + Sync {
    /// Opaque block context type. Matches `IndexDef::Context` for all
    /// registered indexes.
    type BlockContext: Send + Sync;

    /// Configure which source data to fetch, based on the union of all
    /// registered indexes' requirements.
    fn configure(&mut self, requirements: SourceRequirements);

    // NOTE: the streaming `provision` method is intentionally omitted
    // from this initial sketch. It will depend on the async runtime
    // choice (tokio channels, crossbeam, etc.) and the engine's pipeline
    // design. The signature will look roughly like:
    //
    //   async fn provision(
    //       &self,
    //       range: Range<Self::Height>,
    //       tx: Sender<(Self::Height, Self::BlockContext)>,
    //   ) -> Result<(), ProvisionError>;
}
