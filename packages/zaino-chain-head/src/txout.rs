//! What the canonical window did to the transparent UTXO set.
//!
//! The same boundary as the address effects: ChainHead reports what its window
//! can see and never resolves across its floor. An output created in the window
//! is fully described here; a spend of an output created below the window is
//! reported only as the outpoint it spends, because the value and script it
//! removes are held by the finalised state. Joining the two is the consumer's
//! job.

use zaino_primitives::types::{Outpoint, TransparentOutput};

/// An output the window created and has not spent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreatedTxOut {
    /// Where the output sits.
    pub outpoint: Outpoint,
    /// The output itself.
    pub output: TransparentOutput,
}

/// The canonical window's contribution to the UTXO set, from some height up to
/// the tip.
///
/// A spend and the output it spends both inside the range cancel, so neither
/// appears. What is left is what a consumer applies to a set that describes the
/// chain below the range.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChainHeadTxOutDelta {
    /// Outputs created within the range and still unspent at the tip, in chain
    /// order.
    pub created: Vec<CreatedTxOut>,
    /// Outpoints spent within the range whose outputs were created below it,
    /// in chain order.
    pub spent_below: Vec<Outpoint>,
}
