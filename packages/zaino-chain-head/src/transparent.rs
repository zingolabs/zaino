//! Transparent-address effects inside the ChainHead window.
//!
//! **Declared, not implemented.** Nothing here has an implementation and no
//! consumer is wired to it. The module exists to state the boundary while the
//! capability is built: ChainHead contributes the *effects* it can see within
//! its window, and never constructs complete address history.
//!
//! The distinction matters because the two are easy to conflate. A complete
//! answer for an address needs the finalised state's indexes; ChainHead holds
//! only a bounded window, so it can report an output it created and a spend
//! whose output it also created, but it cannot resolve a spend of an output
//! created below its floor — it does not hold that output, and going to look
//! for one would be the historical scan this crate exists to avoid. Joining
//! those cross-boundary spends is the consumer's job.
//!
//! # Addresses are keyed, not encoded
//!
//! Every address here is a
//! [`TransparentAddressKey`](zaino_primitives::types::TransparentAddressKey):
//! a 20-byte hash and the script form it came from, which is what
//! [`classify_script`](zaino_primitives::types::classify_script) produces.
//!
//! Not the wallet-facing string, for two reasons. A window contains outputs
//! with no address at all, and an index that could not name them would report
//! "no history" for an address that has some. And the string is
//! network-specific — the same 20 bytes are `t1` on mainnet and `tm` on testnet
//! — so producing one needs a network the chain head has no other reason to
//! hold, and address encoding belongs to the serving layer.
//!
//! It is the same key the finalised store indexes by, which is the point: a
//! consumer merging the two halves joins them directly. Two equivalent keys in
//! two crates is how the two halves of one answer come to disagree about what
//! they are describing.

use zaino_primitives::types::{
    BlockRef, Height, Outpoint, Script, TransactionId, TransparentAddressKey, TxIndex, Zatoshis,
};

/// Which addresses to report effects for, over which part of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
// Not `non_exhaustive`: this is an input a caller constructs, so sealing it
// would leave no way to build one. The result types below are sealed instead —
// they are ours to extend.
pub struct TransparentHistoryQuery {
    /// The addresses to report on, as an index keys them.
    ///
    /// [`TransparentAddressKey`] rather than the wallet-facing string, for the
    /// two reasons that type exists. A window contains outputs with no address
    /// at all, and naming them is the only way to report on them. And the
    /// string form is network-specific, so producing one needs a network the
    /// chain head has no other reason to hold — encoding is a serving concern.
    ///
    /// It is also the key the finalised store indexes by, which is what makes
    /// the two halves of an address's history joinable without translating.
    pub addresses: Vec<TransparentAddressKey>,
    /// Lowest height to consider, inclusive.
    pub start: Height,
    /// Highest height to consider, inclusive.
    pub end: Height,
}

/// An output created for a queried address, and where it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocatedTransparentOutput {
    /// The address the output pays, as an index keys it.
    pub address: TransparentAddressKey,
    /// The output itself.
    pub outpoint: Outpoint,
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// The output script.
    pub script: Script,
    /// The block that created it.
    pub block: BlockRef,
    /// The creating transaction's index within that block.
    pub tx_index: TxIndex,
}

/// A spend of an output that was itself created inside the window.
///
/// Only locally resolvable spends appear here. A spend of an output created
/// below the retention floor is invisible to ChainHead — it cannot name the
/// address or value being spent, because it does not hold the output that
/// carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocatedTransparentSpend {
    /// The address whose output was spent, as an index keys it.
    pub address: TransparentAddressKey,
    /// The output that was spent.
    pub outpoint: Outpoint,
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// The block containing the spend.
    pub block: BlockRef,
    /// The spending transaction.
    pub txid: TransactionId,
    /// The spending transaction's index within that block.
    pub tx_index: TxIndex,
}

/// What ChainHead can account for, for the queried addresses.
///
/// Deliberately not a balance or a delta list: those are complete-chain
/// answers, and this is a contribution to one. The consumer combines it with
/// the finalised state's indexes to produce the complete answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChainHeadAddressEffects {
    /// Outputs created for the queried addresses inside the window.
    pub outputs: Vec<LocatedTransparentOutput>,
    /// Spends whose referenced output was also created inside the window.
    pub local_spends: Vec<LocatedTransparentSpend>,
}
