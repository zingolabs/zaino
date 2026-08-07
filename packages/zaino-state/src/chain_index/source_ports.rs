//! What ChainIndex requires of a validator, stated as one bound.
//!
//! This is the consumer half of the port. [`zaino_source`] defines the
//! vocabulary — one trait per question a validator can answer — and this names
//! the subset ChainIndex actually asks. The bound lives here rather than in
//! `zaino-source` because it describes a *requirement of this crate*, not a
//! capability of that one: a different consumer needs a different subset, and
//! `zaino-source` should not have to know who its consumers are.
//!
//! Nothing implements this directly. The blanket impl below applies it to any
//! type answering all of the questions, so an adapter earns the bound by
//! implementing the ports it can serve — production composites and test mocks
//! alike.
//!
//! # Relationship to `BlockchainSource`
//!
//! [`BlockchainSource`](super::source::BlockchainSource) is the *driven port*:
//! the shape ChainIndex consumes, still expressed in wire types, and temporary
//! scaffolding.
//! [`ValidatorSource`](super::validator_source::ValidatorSource) is the
//! single adapter between the two — it implements the driven port for any
//! `ChainIndexSourcePorts`. As subsystems move onto the ports directly, methods
//! leave `BlockchainSource` and this bound shrinks with it.

/// Every question ChainIndex asks a validator.
///
/// See the module documentation for why this lives here rather than in
/// `zaino-source`.
pub trait ChainIndexSourcePorts:
    zaino_source::GetAddressBalance
    + zaino_source::GetAddressDeltas
    + zaino_source::GetAddressTxids
    + zaino_source::GetAddressUtxos
    + zaino_source::GetBestBlockHeight
    + zaino_source::GetBlockDeltas
    + zaino_source::GetBlockHeader
    + zaino_source::GetBlockSubsidy
    + zaino_source::GetBlockVerboseByHash
    + zaino_source::GetBlockchainInfo
    + zaino_source::GetChainTip
    + zaino_source::GetChainTips
    + zaino_source::GetCommitmentTreeRoots
    + zaino_source::GetDifficulty
    + zaino_source::GetMempoolMetadata
    + zaino_source::GetMempoolSourceTip
    + zaino_source::GetMempoolTxids
    + zaino_source::GetMiningInfo
    + zaino_source::GetNetworkSolPs
    + zaino_source::GetNodeInfo
    + zaino_source::GetPeerInfo
    + zaino_source::GetRawBlock
    + zaino_source::GetRawBlockByHash
    + zaino_source::GetRawBlockHeader
    + zaino_source::GetRawMempoolTransaction
    + zaino_source::GetSpentInfo
    + zaino_source::GetSubtreeRoots
    + zaino_source::GetTransaction
    + zaino_source::GetTreestate
    + zaino_source::GetTreestateByHash
    + zaino_source::GetTxOut
    + zaino_source::SendRawTransaction
    + zaino_source::SourceLifecycle
    + zaino_source::SubscribeBlocks
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainIndexSourcePorts for T where
    T: zaino_source::GetAddressBalance
        + zaino_source::GetAddressDeltas
        + zaino_source::GetAddressTxids
        + zaino_source::GetAddressUtxos
        + zaino_source::GetBestBlockHeight
        + zaino_source::GetBlockDeltas
        + zaino_source::GetBlockHeader
        + zaino_source::GetBlockSubsidy
        + zaino_source::GetBlockVerboseByHash
        + zaino_source::GetBlockchainInfo
        + zaino_source::GetChainTip
        + zaino_source::GetChainTips
        + zaino_source::GetCommitmentTreeRoots
        + zaino_source::GetDifficulty
        + zaino_source::GetMempoolMetadata
        + zaino_source::GetMempoolSourceTip
        + zaino_source::GetMempoolTxids
        + zaino_source::GetMiningInfo
        + zaino_source::GetNetworkSolPs
        + zaino_source::GetNodeInfo
        + zaino_source::GetPeerInfo
        + zaino_source::GetRawBlock
        + zaino_source::GetRawBlockByHash
        + zaino_source::GetRawBlockHeader
        + zaino_source::GetRawMempoolTransaction
        + zaino_source::GetSpentInfo
        + zaino_source::GetSubtreeRoots
        + zaino_source::GetTransaction
        + zaino_source::GetTreestate
        + zaino_source::GetTreestateByHash
        + zaino_source::GetTxOut
        + zaino_source::SendRawTransaction
        + zaino_source::SourceLifecycle
        + zaino_source::SubscribeBlocks
        + Send
        + Sync
        + 'static
{
}

#[cfg(test)]
mod tests {
    use super::ChainIndexSourcePorts;

    /// The production composite must satisfy the bound. A compile-time check:
    /// if a port is added to ChainIndex's requirements that `ZebraValidator`
    /// cannot answer, this stops building.
    #[test]
    fn zebra_validator_satisfies_the_bound() {
        fn assert_satisfied<T: ChainIndexSourcePorts>() {}
        assert_satisfied::<zaino_source_zebra::ZebraValidator>();
    }
}
