//! Provisioner trait — source data acquisition.
//!
//! The provisioner owns all source access. Indexes never call the source
//! directly. The engine tells the provisioner what data to fetch (via
//! [`SourceRequirements`]), and the provisioner returns block contexts for
//! a requested height range.

use crate::descriptor::SourceRequirements;
use crate::primitives::BlockHeight;

/// Errors from provisioner operations.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The source was unreachable or returned an error.
    #[error("source error: {0}")]
    Source(String),
}

/// The provisioner: first-class owner of all source access.
///
/// Generic — no blockchain knowledge. The `BlockContext` associated type
/// is opaque to the engine; extractors know its concrete type through
/// [`IndexDef::Context`](crate::traits::IndexDef::Context).
///
/// **MVP shape.** The `provision_range` method returns a `Vec` of all
/// block contexts synchronously. The true north is a streaming interface
/// where the provisioner pushes contexts through a bounded channel as
/// they become ready, enabling pipelined extraction.
pub trait Provisioner: Send + Sync {
    /// Opaque block context type. Each index's `IndexDef::BlockContext`
    /// is projected from this type via `ProvideContext`.
    type BlockContext: Send + Sync;

    /// Configure which source data to fetch, based on the union of all
    /// registered indexes' requirements.
    fn configure(&mut self, requirements: SourceRequirements);

    /// Fetch block contexts for a height range (inclusive on both ends).
    fn provision_range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Result<Vec<Self::BlockContext>, ProvisionError>;
}
