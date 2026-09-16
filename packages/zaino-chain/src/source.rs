//! What a chain view asks of a validator.

/// Everything a chain view asks of a validator.
///
/// An alias over `zaino-source`, not a new port — the same shape
/// [`ChainStoreSource`](zaino_chain_store::ChainStoreSource) and
/// [`ChainHeadBlockSource`](zaino_chain_head::ChainHeadBlockSource) take. It
/// states a requirement of *this* consumer; `zaino-source` should not have to
/// know who its consumers are, so the list lives here rather than there.
///
/// # Why both addressings are here
///
/// Nearly every question appears twice, by height and by hash, and that is
/// deliberate rather than redundant. A validator answers either — zebra takes a
/// `HashOrHeight` — and which one a chain view should use depends on what it
/// already knows:
///
/// - **By hash** where a tier covered the height, so the hash was resolved
///   locally and costs nothing. Exact, and reorg-stable inside a pinned view.
/// - **By height** where no tier covered it. There is no local hash to resolve
///   against — that is what a hole *is* — and a hole sits below the chain
///   head's retention floor, hence below the reorg bound, so a by-height read
///   there is stable anyway.
///
/// Restricting this port to by-hash reads would not buy coherence; it would
/// only make the reads that matter most during catch-up impossible, and force a
/// wasted round trip on the rest.
pub trait ChainViewSource:
    // Blocks, both addressings.
    zaino_source::OneShotGetBlock
    + zaino_source::OneShotGetBlockByHash
    + zaino_source::OneShotGetRawBlock
    + zaino_source::OneShotGetRawBlockByHash
    // The compact projection, so filling a hole in a wallet-sync range does not
    // pay to transfer and parse whole blocks.
    + zaino_source::OneShotGetPreIndexCompactBlock
    + zaino_source::OneShotGetBlockVerbose
    + zaino_source::OneShotGetCommitmentTreeRoots
    // Consensus bytes and commitment trees, which no tier retains.
    + zaino_source::OneShotGetTransaction
    + zaino_source::OneShotGetTreestate
    + zaino_source::OneShotGetTreestateByHash
    + zaino_source::OneShotGetSubtreeRoots
    // Transparent address history, which Zaino does not index today.
    + zaino_source::OneShotGetAddressBalance
    + zaino_source::OneShotGetAddressUtxos
    + zaino_source::OneShotGetAddressTxids
    + zaino_source::OneShotGetAddressDeltas
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainViewSource for T where
    T: zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetBlockByHash
        + zaino_source::OneShotGetRawBlock
        + zaino_source::OneShotGetRawBlockByHash
        + zaino_source::OneShotGetPreIndexCompactBlock
        + zaino_source::OneShotGetBlockVerbose
        + zaino_source::OneShotGetCommitmentTreeRoots
        + zaino_source::OneShotGetTransaction
        + zaino_source::OneShotGetTreestate
        + zaino_source::OneShotGetTreestateByHash
        + zaino_source::OneShotGetSubtreeRoots
        + zaino_source::OneShotGetAddressBalance
        + zaino_source::OneShotGetAddressUtxos
        + zaino_source::OneShotGetAddressTxids
        + zaino_source::OneShotGetAddressDeltas
        + Send
        + Sync
        + 'static
{
}

#[cfg(test)]
mod tests {
    use super::ChainViewSource;

    /// A real validator satisfies the source bound.
    ///
    /// The bound is a list of questions, and nothing else checks that some
    /// adapter can answer all of them — a port naming a capability no source
    /// provides would compile perfectly well and fail at wiring time. The same
    /// assertion guards `ChainStoreSource` and `ChainHeadBlockSource`.
    #[test]
    fn the_zebra_validator_satisfies_the_source_bound() {
        fn assert_satisfied<T: ChainViewSource>() {}
        assert_satisfied::<zaino_source_zebra::ZebraValidator>();
    }
}
