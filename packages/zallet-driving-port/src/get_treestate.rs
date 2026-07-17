//! Capability: read the per-pool treestate as of a height.

use std::future::Future;

use zaino_primitives::types::Height;

use crate::error::PortError;
use crate::treestate_at::TreestateAt;

/// Domain error for [`GetTreestate`].
///
/// Empty: a height beyond the pinned tip is an answer (`Ok(None)`),
/// not a rejection, and an absent pool frontier is carried inside
/// [`TreestateAt`] as an empty tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GetTreestateError {}

/// Read the note commitment tree state of every shielded pool as of a
/// height in the pinned view.
///
/// Every in-view height has a treestate; what varies per pool is
/// whether the frontier is present, and absence means an empty tree
/// (zcash/zallet#455).
pub trait GetTreestate: Send + Sync {
    /// The treestate as of `height`, or `None` when the height lies
    /// beyond the pinned tip.
    fn get_treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<TreestateAt>, PortError<GetTreestateError>>> + Send;
}
