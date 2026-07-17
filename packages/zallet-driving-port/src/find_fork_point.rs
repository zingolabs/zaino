//! Capability: locate the fork point between a driver's view and the
//! pinned best chain.

use std::future::Future;

use crate::block_id::BlockId;
use crate::block_locator::BlockLocator;
use crate::error::PortError;

/// Domain error for [`FindForkPoint`].
///
/// Empty: a locator sharing no block with the pinned chain is an
/// answer (`Ok(None)`), not a rejection, and locator well-formedness is
/// enforced by [`BlockLocator`]'s constructor before the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FindForkPointError {}

/// Locate the fork point between a driver's view and the pinned best
/// chain.
///
/// The fork point is the locator entry whose block sits highest on the
/// pinned chain. Membership is judged by hash — never by the heights
/// the locator claims — and the returned [`BlockId`] carries the
/// pinned chain's height for that hash. Everything the driver holds
/// above the fork point is not on the pinned chain.
pub trait FindForkPoint: Send + Sync {
    /// The highest locator entry on the pinned chain, or `None` when
    /// the views share no block.
    fn find_fork_point(
        &self,
        locator: &BlockLocator,
    ) -> impl Future<Output = Result<Option<BlockId>, PortError<FindForkPointError>>> + Send;
}
