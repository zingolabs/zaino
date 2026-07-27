//! Provisioner trait — source data acquisition.
//!
//! The provisioner owns all source access. Indexes never call the source
//! directly. The provisioner's output type determines what data is
//! available — indexes access it through [`ProvideContext`] projections.
//!
//! [`ProvideContext`]: crate::traits::ProvideContext

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
/// is the set-wide context that each index's
/// [`BlockContext`](crate::traits::IndexDef::BlockContext) is projected
/// from via [`ProvideContext`](crate::traits::ProvideContext).
///
/// The provisioner is purpose-built for each index set. Its output type
/// contains exactly the data the set's indexes need — fields that no
/// index projects into are dead code, signalling unnecessary RPC calls.
///
/// **MVP shape.** The `provision_range` method returns a `Vec` of all
/// block contexts synchronously. The true north is a streaming interface
/// where the provisioner pushes contexts through a bounded channel as
/// they become ready, enabling pipelined extraction.
pub trait Provisioner: Send + Sync {
    /// Set-wide block context type. Each index's `IndexDef::BlockContext`
    /// is projected from this type via `ProvideContext`.
    type BlockContext: Send + Sync;

    /// Fetch block contexts for a height range (inclusive on both ends).
    fn provision_range(
        &self,
        from: BlockHeight,
        to: BlockHeight,
    ) -> Result<Vec<Self::BlockContext>, ProvisionError>;
}
