//! What each subsystem needs from the backing validator.
//!
//! Every consumer here used to be generic over one `BlockchainSource` bound,
//! which said nothing about what that consumer actually asked the validator.
//! The port is now one trait per question, so a bound can say exactly that —
//! but writing the full list at every `impl` would be unreadable, so each
//! subsystem gets an alias naming its own set.
//!
//! These live beside the consumers rather than in `zaino-source`. An alias
//! called `FinalisedSourceCaps` describes a subsystem of *this* crate; putting
//! it in the port crate would make the ports know the internal structure of
//! their consumer, which is the dependency direction the split exists to
//! establish.
//!
//! Each alias is a supertrait with a blanket impl, so any source satisfying the
//! listed capabilities satisfies the alias automatically.

use zaino_source::*;

/// What the finalised state asks of the validator while syncing.
///
/// Blocks come as canonical bytes rather than parsed
/// ([`GetRawBlock`]): the finalised state builds its own indexed
/// representation from them, and needs the exact bytes the block hash commits
/// to rather than a shape something else has already interpreted.
pub trait FinalisedSourceCaps:
    GetBestBlockHeight + GetRawBlock + GetCommitmentTreeRoots + GetTransaction + Send + Sync + 'static
{
}

impl<T> FinalisedSourceCaps for T where
    T: GetBestBlockHeight
        + GetRawBlock
        + GetCommitmentTreeRoots
        + GetTransaction
        + Send
        + Sync
        + 'static
{
}

/// What the non-finalised state asks of the validator.
///
/// Needs blocks by hash as well as by height: it tracks competing branches, and
/// a hash is the only way to name a block that is not on the best chain.
pub trait ChainHeadSourceCaps:
    GetChainTip
    + GetRawBlock
    + GetRawBlockByHash
    + GetCommitmentTreeRoots
    + GetChainTips
    + Send
    + Sync
    + 'static
{
}

impl<T> ChainHeadSourceCaps for T where
    T: GetChainTip
        + GetRawBlock
        + GetRawBlockByHash
        + GetCommitmentTreeRoots
        + GetChainTips
        + Send
        + Sync
        + 'static
{
}

/// What the mempool asks of the validator.
///
/// Deliberately narrow, and deliberately not satisfied by a state-database
/// adapter: the mempool is reachable only over JSON-RPC, and this bound is what
/// makes that a compile-time fact rather than a comment.
pub trait MempoolSourceCaps: GetMempoolTxids + GetChainTip + Send + Sync + 'static {}

impl<T> MempoolSourceCaps for T where T: GetMempoolTxids + GetChainTip + Send + Sync + 'static {}

/// What the indexer service asks of the validator.
pub trait IndexerSourceCaps: SubscribeChainTip + Send + Sync + 'static {}

impl<T> IndexerSourceCaps for T where T: SubscribeChainTip + Send + Sync + 'static {}

/// What `ChainIndex` asks of the validator.
///
/// Much the widest of these, and honestly so: `ChainIndex` is the RPC-facing
/// layer, so it forwards every query the index cannot answer locally. That the
/// list is long is information — it says this layer is where the passthrough
/// surface lives, and shrinking it is what the later modularisation is for.
pub trait ChainIndexSourceCaps:
    FinalisedSourceCaps
    + ChainHeadSourceCaps
    + MempoolSourceCaps
    + GetBlock
    + GetBlockVerbose
    + GetBlockHeader
    + GetRawBlockHeader
    + GetBlockDeltas
    + GetBlockSubsidy
    + GetBlockchainInfo
    + GetDifficulty
    + GetNodeInfo
    + GetPeerInfo
    + GetMiningInfo
    + GetNetworkSolPs
    + GetTxOut
    + GetSpentInfo
    + GetSubtreeRoots
    + GetTreestate
    + GetTreestateByHash
    + GetAddressBalance
    + GetAddressDeltas
    + GetAddressTxids
    + GetAddressUtxos
    + SendRawTransaction
    + SubscribeBlocks
    + SourceLifecycle
{
}

impl<T> ChainIndexSourceCaps for T where
    T: FinalisedSourceCaps
        + ChainHeadSourceCaps
        + MempoolSourceCaps
        + GetBlock
        + GetBlockVerbose
        + GetBlockHeader
        + GetRawBlockHeader
        + GetBlockDeltas
        + GetBlockSubsidy
        + GetBlockchainInfo
        + GetDifficulty
        + GetNodeInfo
        + GetPeerInfo
        + GetMiningInfo
        + GetNetworkSolPs
        + GetTxOut
        + GetSpentInfo
        + GetSubtreeRoots
        + GetTreestate
        + GetTreestateByHash
        + GetAddressBalance
        + GetAddressDeltas
        + GetAddressTxids
        + GetAddressUtxos
        + SendRawTransaction
        + SubscribeBlocks
        + SourceLifecycle
{
}
