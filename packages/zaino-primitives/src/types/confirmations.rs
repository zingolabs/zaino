//! Tip-relative confirmation state — for blocks, and for transactions.
//!
//! The JSON-RPC interface flattens this state into one signed integer: `-1`
//! means "not on the best chain", `0` means "in the mempool", and `n ≥ 1`
//! means "on the best chain, at depth `n − 1`". That packing destroys the
//! state — downstream code recovers it by inspecting the sign — so in the
//! domain the state is an enum and the integer exists only at the wire.
//!
//! # Two types, not one
//!
//! The state spaces differ by subject. A block is never in the mempool, so a
//! single three-state enum would force every block-side consumer to write a
//! dead `Mempool` arm (this codebase matches exhaustively, without
//! wildcards). Each type carries exactly its subject's states:
//!
//! - [`BlockConfirmations`] — a block is on the best chain (with a strictly
//!   positive count) or it is not.
//! - [`TxConfirmations`] — a transaction is in the mempool, or it is mined,
//!   in which case its confirmation state *is* its block's.
//!
//! # Composition, not a trait
//!
//! The sharing between the two is vertical — a mined transaction's state is
//! literally a [`BlockConfirmations`] — so [`TxConfirmations`] embeds the
//! block type in its `Mined` variant and forwards behaviour through it. There
//! is deliberately no trait over the two: no consumer is generic over both,
//! so a trait would be an abstraction with no caller. The extraction trigger
//! is documented on [`TxConfirmations`]: if a generic consumer ever appears,
//! extract the trait then — the inherent signatures already align.
//!
//! # The wire scheme
//!
//! The sentinel scheme is written exactly twice — once per type, as a
//! `to_rpc_i64` / `try_from_rpc_i64` codec pair. Rendering is infallible;
//! parsing is the external-input validation step and rejects every integer
//! that encodes no state (`0` on the block door, anything below `-1`, and
//! counts past `u32` on both).
//!
//! # The clamp contract
//!
//! [`BlockConfirmations::of_best_chain_block`] clamps a height above the tip
//! to depth `0`, answering `Confirmed(1)` — see its docs for the contract.

use core::fmt;
use core::num::NonZeroU32;

use super::Height;

/// The wire encoding of [`BlockConfirmations::NotInBestChain`].
const NOT_IN_BEST_CHAIN: i64 = -1;

/// The wire encoding of [`TxConfirmations::Mempool`].
const MEMPOOL: i64 = 0;

/// Tip-relative confirmation state of a block.
///
/// A block has exactly two states: on the best chain — confirmed, with a
/// count that is strictly positive by construction (the block itself is its
/// own first confirmation) — or not on it. There is no mempool state for a
/// block; a type that carried one would force dead match arms on every
/// consumer. See the [module docs](self) for the state model and the wire
/// scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockConfirmations {
    /// The block is not on the best chain. Wire form `-1`.
    NotInBestChain,
    /// The block is on the best chain, `count` confirmations deep — its depth
    /// below the tip, plus one for the block itself. Wire form `n ≥ 1`.
    Confirmed(NonZeroU32),
}

/// Error when an RPC `confirmations` integer encodes no state.
///
/// Shared by both codec doors —
/// [`BlockConfirmations::try_from_rpc_i64`] and
/// [`TxConfirmations::try_from_rpc_i64`]. [`ZeroForBlock`](Self::ZeroForBlock)
/// only comes from the block door: the transaction door maps `0` to
/// [`TxConfirmations::Mempool`] before delegating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfirmationsCodecError {
    /// `0` encodes the mempool state, which a block does not have.
    #[error("confirmations 0 encodes the mempool state, which a block does not have")]
    ZeroForBlock,

    /// Below the `-1` sentinel — the scheme assigns no meaning there.
    #[error("confirmations {got} is below the -1 sentinel and encodes no state")]
    BelowSentinel {
        /// The value that was rejected.
        got: i64,
    },

    /// A confirmation count past `u32::MAX`, which no chain can reach.
    #[error("confirmation count {got} does not fit u32")]
    CountOverflow {
        /// The value that was rejected.
        got: i64,
    },
}

impl BlockConfirmations {
    /// The confirmation count, or `None` when the block is not on the best
    /// chain.
    pub fn count(self) -> Option<NonZeroU32> {
        match self {
            Self::NotInBestChain => None,
            Self::Confirmed(count) => Some(count),
        }
    }

    /// Whether the block is on the best chain.
    pub fn is_in_best_chain(self) -> bool {
        match self {
            Self::NotInBestChain => false,
            Self::Confirmed(_) => true,
        }
    }

    /// The confirmation state of a best-chain block at `height` against the
    /// current `tip`: confirmed, `depth + 1` times.
    ///
    /// The single home for the off-by-one — the block itself is its first
    /// confirmation, so the tip is `Confirmed(1)`, not `Confirmed(0)`. The
    /// depth comes from [`Height::depth_from`].
    ///
    /// # The clamp contract
    ///
    /// A `height` above `tip` — a caller racing a tip update — clamps to
    /// depth `0` and answers `Confirmed(1)`, never an error and never a
    /// non-positive count. This preserves the observable behaviour of the
    /// saturating subtraction it replaces. A caller for whom "above the tip"
    /// is a real state, not a race, must not use this constructor —
    /// [`Height::depth_from`] exposes the `None` it needs.
    pub fn of_best_chain_block(height: Height, tip: Height) -> Self {
        let depth = height.depth_from(tip).unwrap_or(0);
        // depth ≤ tip ≤ 2^31 − 1, so adding 1 cannot saturate.
        Self::Confirmed(NonZeroU32::MIN.saturating_add(depth))
    }

    /// Renders the state in the RPC integer scheme: `-1` for not on the best
    /// chain, the count otherwise.
    pub fn to_rpc_i64(self) -> i64 {
        match self {
            Self::NotInBestChain => NOT_IN_BEST_CHAIN,
            Self::Confirmed(count) => i64::from(count.get()),
        }
    }

    /// Parses an RPC integer as a block's confirmation state.
    ///
    /// The external-input validation step for a `confirmations` field
    /// reported for a block: `-1` is [`NotInBestChain`](Self::NotInBestChain),
    /// `n ≥ 1` is [`Confirmed`](Self::Confirmed). `0` — the mempool state,
    /// which a block does not have — anything below `-1`, and counts past
    /// `u32` encode no state and are rejected.
    pub fn try_from_rpc_i64(value: i64) -> Result<Self, ConfirmationsCodecError> {
        match value {
            NOT_IN_BEST_CHAIN => Ok(Self::NotInBestChain),
            MEMPOOL => Err(ConfirmationsCodecError::ZeroForBlock),
            got if got < NOT_IN_BEST_CHAIN => Err(ConfirmationsCodecError::BelowSentinel { got }),
            got => u32::try_from(got)
                .ok()
                .and_then(NonZeroU32::new)
                .map(Self::Confirmed)
                .ok_or(ConfirmationsCodecError::CountOverflow { got }),
        }
    }
}

impl fmt::Display for BlockConfirmations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInBestChain => write!(f, "not in best chain"),
            Self::Confirmed(count) => write!(f, "{count} confirmations"),
        }
    }
}

/// Tip-relative confirmation state of a transaction (and its outputs).
///
/// A transaction adds one state a block does not have — the mempool — and a
/// mined transaction's confirmation state *is* its block's, so the block
/// state is embedded rather than restated. See the [module docs](self) for
/// why this is composition and not a shared trait.
///
/// # Extraction trigger
///
/// There is deliberately no trait over this type and
/// [`BlockConfirmations`]: no consumer is generic over both. If one ever
/// appears, extract a trait then — the inherent signatures already align.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxConfirmations {
    /// The transaction is in the mempool. Wire form `0`.
    Mempool,
    /// The transaction is mined; its confirmation state is its block's.
    Mined(BlockConfirmations),
}

impl TxConfirmations {
    /// The confirmation count, or `None` when the transaction is unconfirmed
    /// — in the mempool, or mined on a block off the best chain.
    pub fn count(self) -> Option<NonZeroU32> {
        match self {
            Self::Mempool => None,
            Self::Mined(block) => block.count(),
        }
    }

    /// Whether the transaction is mined on the best chain.
    pub fn is_in_best_chain(self) -> bool {
        match self {
            Self::Mempool => false,
            Self::Mined(block) => block.is_in_best_chain(),
        }
    }

    /// Renders the state in the RPC integer scheme: `0` for the mempool,
    /// the block's rendering otherwise.
    pub fn to_rpc_i64(self) -> i64 {
        match self {
            Self::Mempool => MEMPOOL,
            Self::Mined(block) => block.to_rpc_i64(),
        }
    }

    /// Parses an RPC integer as a transaction's confirmation state.
    ///
    /// `0` is [`Mempool`](Self::Mempool); every other value delegates to
    /// [`BlockConfirmations::try_from_rpc_i64`], so the same integers are
    /// rejected — anything below `-1`, and counts past `u32`.
    /// [`ConfirmationsCodecError::ZeroForBlock`] never comes from this door.
    pub fn try_from_rpc_i64(value: i64) -> Result<Self, ConfirmationsCodecError> {
        match value {
            MEMPOOL => Ok(Self::Mempool),
            mined => BlockConfirmations::try_from_rpc_i64(mined).map(Self::Mined),
        }
    }
}

impl fmt::Display for TxConfirmations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mempool => write!(f, "in mempool"),
            Self::Mined(block) => block.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmed(count: u32) -> BlockConfirmations {
        BlockConfirmations::Confirmed(NonZeroU32::new(count).expect("count is non-zero"))
    }

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid height")
    }

    #[test]
    fn block_codec_round_trips() {
        for state in [
            BlockConfirmations::NotInBestChain,
            confirmed(1),
            confirmed(2_000_000),
            confirmed(u32::MAX),
        ] {
            assert_eq!(
                BlockConfirmations::try_from_rpc_i64(state.to_rpc_i64()),
                Ok(state)
            );
        }
    }

    #[test]
    fn block_wire_values() {
        assert_eq!(BlockConfirmations::NotInBestChain.to_rpc_i64(), -1);
        assert_eq!(confirmed(1).to_rpc_i64(), 1);
        assert_eq!(confirmed(7).to_rpc_i64(), 7);
    }

    #[test]
    fn block_door_rejects_zero() {
        assert_eq!(
            BlockConfirmations::try_from_rpc_i64(0),
            Err(ConfirmationsCodecError::ZeroForBlock)
        );
    }

    #[test]
    fn block_door_rejects_below_sentinel() {
        assert_eq!(
            BlockConfirmations::try_from_rpc_i64(-7),
            Err(ConfirmationsCodecError::BelowSentinel { got: -7 })
        );
        assert_eq!(
            BlockConfirmations::try_from_rpc_i64(i64::MIN),
            Err(ConfirmationsCodecError::BelowSentinel { got: i64::MIN })
        );
    }

    #[test]
    fn block_door_rejects_count_past_u32() {
        let over = i64::from(u32::MAX) + 1;
        assert_eq!(
            BlockConfirmations::try_from_rpc_i64(over),
            Err(ConfirmationsCodecError::CountOverflow { got: over })
        );
        assert_eq!(
            BlockConfirmations::try_from_rpc_i64(i64::MAX),
            Err(ConfirmationsCodecError::CountOverflow { got: i64::MAX })
        );
    }

    #[test]
    fn tx_codec_round_trips() {
        for state in [
            TxConfirmations::Mempool,
            TxConfirmations::Mined(BlockConfirmations::NotInBestChain),
            TxConfirmations::Mined(confirmed(1)),
            TxConfirmations::Mined(confirmed(u32::MAX)),
        ] {
            assert_eq!(
                TxConfirmations::try_from_rpc_i64(state.to_rpc_i64()),
                Ok(state)
            );
        }
    }

    #[test]
    fn tx_door_maps_zero_to_mempool() {
        assert_eq!(
            TxConfirmations::try_from_rpc_i64(0),
            Ok(TxConfirmations::Mempool)
        );
    }

    #[test]
    fn tx_door_rejects_what_the_block_door_rejects() {
        assert_eq!(
            TxConfirmations::try_from_rpc_i64(-2),
            Err(ConfirmationsCodecError::BelowSentinel { got: -2 })
        );
        let over = i64::from(u32::MAX) + 1;
        assert_eq!(
            TxConfirmations::try_from_rpc_i64(over),
            Err(ConfirmationsCodecError::CountOverflow { got: over })
        );
    }

    #[test]
    fn tip_block_has_one_confirmation() {
        let tip = height(100);
        assert_eq!(
            BlockConfirmations::of_best_chain_block(tip, tip),
            confirmed(1)
        );
    }

    #[test]
    fn depth_shifts_to_count_by_one() {
        assert_eq!(
            BlockConfirmations::of_best_chain_block(height(97), height(100)),
            confirmed(4)
        );
    }

    #[test]
    fn height_above_tip_clamps_to_one_confirmation() {
        assert_eq!(
            BlockConfirmations::of_best_chain_block(height(101), height(100)),
            confirmed(1)
        );
    }

    #[test]
    fn block_accessors() {
        assert_eq!(BlockConfirmations::NotInBestChain.count(), None);
        assert!(!BlockConfirmations::NotInBestChain.is_in_best_chain());
        assert_eq!(confirmed(3).count(), NonZeroU32::new(3));
        assert!(confirmed(3).is_in_best_chain());
    }

    #[test]
    fn tx_forwards_through_mined() {
        assert_eq!(TxConfirmations::Mempool.count(), None);
        assert!(!TxConfirmations::Mempool.is_in_best_chain());

        let off_chain = TxConfirmations::Mined(BlockConfirmations::NotInBestChain);
        assert_eq!(off_chain.count(), None);
        assert!(!off_chain.is_in_best_chain());

        let mined = TxConfirmations::Mined(confirmed(5));
        assert_eq!(mined.count(), NonZeroU32::new(5));
        assert!(mined.is_in_best_chain());
    }
}
