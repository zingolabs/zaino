//! The port impls: how each question is placed across the members.
//!
//! Three placements, one macro each. A macro rather than a function because a
//! function cannot express it: the call dispatches one method name over a
//! generic member and returns a future borrowing that member, which no closure
//! signature in stable Rust can name.
//!
//! - [`spread!`] — chain reads: start at the round-robin cursor, move to the
//!   next member on a failure or a miss; a miss is the answer only when every
//!   member reported one.
//! - [`first!`] — passthrough questions with an authoritative domain answer:
//!   the first member that answers, its domain answer returned as given.
//! - [`pinned!`] — the mempool: one member until it fails, so a listing, its
//!   transactions and its tip come from one mempool.

use std::convert::Infallible;
use std::sync::atomic::Ordering;

use tracing::debug;
use zaino_primitives::types::{
    rpc, AddressBalance, AddressDelta, Block, BlockHash, BlockVerbose, BlockchainInfo, Difficulty,
    Height, OutputIndex, PreIndexCompactBlock, PreIndexCompactTx, ShieldedPool, SubtreeRoot,
    TransactionId, TreeRoots, Treestate, Utxo,
};

use super::Quorum;
use crate::{
    FailureMode, GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError,
    GetAddressUtxosError, GetBestBlockHeightError, GetBlockByHashError, GetBlockDeltasError,
    GetBlockError, GetBlockHeaderError, GetBlockSubsidyError, GetBlockVerboseError,
    GetBlockchainInfoError, GetChainTipError, GetChainTipsError, GetCommitmentTreeRootsError,
    GetDifficultyError, GetMempoolMetadataError, GetMempoolTxidsError, GetMiningInfoError,
    GetNetworkSolPsError, GetNodeInfoError, GetPeerInfoError, GetRawMempoolTransactionError,
    GetSpentInfoError, GetSubtreeRootsError, GetTransactionError, GetTreestateByHashError,
    GetTreestateError, GetTxOutError, MempoolTxMeta, NonDomainError, OneShotGetAddressBalance,
    OneShotGetAddressDeltas, OneShotGetAddressTxids, OneShotGetAddressUtxos,
    OneShotGetBestBlockHeight, OneShotGetBlock, OneShotGetBlockByHash, OneShotGetBlockDeltas,
    OneShotGetBlockHeader, OneShotGetBlockSubsidy, OneShotGetBlockVerbose,
    OneShotGetBlockVerboseByHash, OneShotGetBlockchainInfo, OneShotGetChainTip,
    OneShotGetChainTips, OneShotGetCommitmentTreeRoots, OneShotGetDifficulty,
    OneShotGetMempoolCompactTransaction, OneShotGetMempoolMetadata, OneShotGetMempoolSourceTip,
    OneShotGetMempoolTxids, OneShotGetMiningInfo, OneShotGetNetworkSolPs, OneShotGetNodeInfo,
    OneShotGetPeerInfo, OneShotGetPreIndexCompactBlock, OneShotGetRawBlock,
    OneShotGetRawBlockByHash, OneShotGetRawBlockHeader, OneShotGetRawMempoolTransaction,
    OneShotGetSpentInfo, OneShotGetSubtreeRoots, OneShotGetTransaction, OneShotGetTreestate,
    OneShotGetTreestateByHash, OneShotGetTxOut, OneShotSendRawTransaction, QueryError,
    SendRawTransactionError, TransactionResponse,
};

/// The failure a call reports when it ran out of members without any of them
/// failing — impossible with at least one member, which construction
/// guarantees, but the macros need a value for the exhausted case.
fn no_members() -> NonDomainError {
    NonDomainError::new(FailureMode::Connection, "quorum has no members")
}

/// Spread: try each member once from the round-robin cursor; a failure or a
/// miss moves on. The miss is returned only when every member reported one.
macro_rules! spread {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let shared = &$self.shared;
        let count = shared.members.len();
        let start = shared.cursor.fetch_add(1, Ordering::Relaxed) % count;
        let mut miss = None;
        let mut failure: Option<NonDomainError> = None;
        for offset in 0..count {
            let index = (start + offset) % count;
            match shared.members[index].$method($($arg.clone()),*).await {
                Ok(answer) => return Ok(answer),
                Err(QueryError::Domain(m)) => {
                    debug!(member = index, miss = %m, "member has no answer; trying the next");
                    miss = Some(m);
                }
                Err(QueryError::NonDomain(f)) => {
                    let f: NonDomainError = f.into();
                    debug!(member = index, error = %f, "member failed; trying the next");
                    failure = Some(f);
                }
            }
        }
        Err(match (failure, miss) {
            (Some(f), _) => QueryError::NonDomain(f),
            (None, Some(m)) => QueryError::Domain(m),
            (None, None) => QueryError::NonDomain(no_members()),
        })
    }};
}

/// First: the first member that answers; its domain answer is authoritative.
macro_rules! first {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let shared = &$self.shared;
        let count = shared.members.len();
        let start = shared.cursor.fetch_add(1, Ordering::Relaxed) % count;
        let mut failure: Option<NonDomainError> = None;
        for offset in 0..count {
            let index = (start + offset) % count;
            match shared.members[index].$method($($arg.clone()),*).await {
                Ok(answer) => return Ok(answer),
                Err(QueryError::Domain(d)) => return Err(QueryError::Domain(d)),
                Err(QueryError::NonDomain(f)) => {
                    let f: NonDomainError = f.into();
                    debug!(member = index, error = %f, "member failed; trying the next");
                    failure = Some(f);
                }
            }
        }
        Err(QueryError::NonDomain(failure.unwrap_or_else(no_members)))
    }};
}

/// Pinned: the mempool member; a failure moves the pin to the next member.
macro_rules! pinned {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let shared = &$self.shared;
        let count = shared.members.len();
        let mut failure: Option<NonDomainError> = None;
        for _ in 0..count {
            let index = shared.mempool_member.load(Ordering::Relaxed) % count;
            match shared.members[index].$method($($arg.clone()),*).await {
                Ok(answer) => return Ok(answer),
                Err(QueryError::Domain(d)) => return Err(QueryError::Domain(d)),
                Err(QueryError::NonDomain(f)) => {
                    let f: NonDomainError = f.into();
                    debug!(member = index, error = %f, "mempool member failed; pinning the next");
                    // Move the pin off the member that failed; a concurrent
                    // call that moved it already leaves it where it is.
                    let _ = shared.mempool_member.compare_exchange(
                        index,
                        (index + 1) % count,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    failure = Some(f);
                }
            }
        }
        Err(QueryError::NonDomain(failure.unwrap_or_else(no_members)))
    }};
}

// ---------------------------------------------------------------------------
// The tip: agreement
// ---------------------------------------------------------------------------

impl<A: OneShotGetChainTip> OneShotGetChainTip for Quorum<A> {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        self.shared.agreed_tip().await
    }
}

impl<A: OneShotGetChainTip> OneShotGetBestBlockHeight for Quorum<A> {
    async fn get_best_block_height(&self) -> Result<Height, QueryError<GetBestBlockHeightError>> {
        // The best height is the agreed tip's, never one member's opinion.
        match self.shared.agreed_tip().await {
            Ok((_, height)) => Ok(height),
            Err(QueryError::Domain(GetChainTipError::NotReady)) => {
                Err(QueryError::Domain(GetBestBlockHeightError::NotReady))
            }
            Err(QueryError::NonDomain(failure)) => Err(QueryError::NonDomain(failure)),
        }
    }
}

// ---------------------------------------------------------------------------
// Chain reads: spread
// ---------------------------------------------------------------------------

impl<A: OneShotGetBlock> OneShotGetBlock for Quorum<A> {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        spread!(self, get_block, height)
    }
}

impl<A: OneShotGetBlockByHash> OneShotGetBlockByHash for Quorum<A> {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        spread!(self, get_block_by_hash, hash)
    }
}

impl<A: OneShotGetRawBlock> OneShotGetRawBlock for Quorum<A> {
    async fn get_raw_block(&self, height: Height) -> Result<Vec<u8>, QueryError<GetBlockError>> {
        spread!(self, get_raw_block, height)
    }
}

impl<A: OneShotGetRawBlockByHash> OneShotGetRawBlockByHash for Quorum<A> {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        spread!(self, get_raw_block_by_hash, hash)
    }
}

impl<A: OneShotGetPreIndexCompactBlock> OneShotGetPreIndexCompactBlock for Quorum<A> {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<PreIndexCompactBlock, QueryError<GetBlockError>> {
        spread!(self, get_pre_index_compact_block, height)
    }
}

impl<A: OneShotGetBlockHeader> OneShotGetBlockHeader for Quorum<A> {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockHeaderVerbose, QueryError<GetBlockHeaderError>> {
        spread!(self, get_block_header, hash)
    }
}

impl<A: OneShotGetRawBlockHeader> OneShotGetRawBlockHeader for Quorum<A> {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockHeaderError>> {
        spread!(self, get_raw_block_header, hash)
    }
}

impl<A: OneShotGetBlockVerbose> OneShotGetBlockVerbose for Quorum<A> {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        spread!(self, get_block_verbose, height)
    }
}

impl<A: OneShotGetBlockVerboseByHash> OneShotGetBlockVerboseByHash for Quorum<A> {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        spread!(self, get_block_verbose_by_hash, hash)
    }
}

impl<A: OneShotGetCommitmentTreeRoots> OneShotGetCommitmentTreeRoots for Quorum<A> {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        spread!(self, get_commitment_tree_roots, block)
    }
}

impl<A: OneShotGetTreestate> OneShotGetTreestate for Quorum<A> {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        spread!(self, get_treestate, height)
    }
}

impl<A: OneShotGetTreestateByHash> OneShotGetTreestateByHash for Quorum<A> {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<GetTreestateByHashError>> {
        spread!(self, get_treestate_by_hash, hash)
    }
}

impl<A: OneShotGetSubtreeRoots> OneShotGetSubtreeRoots for Quorum<A> {
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        spread!(self, get_subtree_roots, pool, start_index, limit)
    }
}

impl<A: OneShotGetTransaction> OneShotGetTransaction for Quorum<A> {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        // A transaction one member has not seen may be mined or in the
        // mempool of another, so a miss moves on.
        spread!(self, get_transaction, txid)
    }
}

impl<A: OneShotGetBlockDeltas> OneShotGetBlockDeltas for Quorum<A> {
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<rpc::BlockDeltas, QueryError<GetBlockDeltasError>> {
        spread!(self, get_block_deltas, hash)
    }
}

impl<A: OneShotGetBlockSubsidy> OneShotGetBlockSubsidy for Quorum<A> {
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<rpc::BlockSubsidy, QueryError<GetBlockSubsidyError>> {
        spread!(self, get_block_subsidy, height)
    }
}

impl<A: OneShotGetTxOut> OneShotGetTxOut for Quorum<A> {
    async fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<rpc::TxOut>, QueryError<GetTxOutError>> {
        spread!(self, get_tx_out, txid, index, include_mempool)
    }
}

impl<A: OneShotGetSpentInfo> OneShotGetSpentInfo for Quorum<A> {
    async fn get_spent_info(
        &self,
        outpoint: rpc::SpentOutpoint,
    ) -> Result<rpc::SpentInfo, QueryError<GetSpentInfoError>> {
        spread!(self, get_spent_info, outpoint)
    }
}

// ---------------------------------------------------------------------------
// Passthrough questions: first member that answers
// ---------------------------------------------------------------------------

impl<A: OneShotSendRawTransaction> OneShotSendRawTransaction for Quorum<A> {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        // A member's rejection is the network's answer; a member that could
        // not be reached did not see the transaction, so the next one is
        // asked with the same bytes.
        first!(self, send_raw_transaction, transaction)
    }
}

impl<A: OneShotGetAddressBalance> OneShotGetAddressBalance for Quorum<A> {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        first!(self, get_address_balance, addresses)
    }
}

impl<A: OneShotGetAddressUtxos> OneShotGetAddressUtxos for Quorum<A> {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        first!(self, get_address_utxos, addresses)
    }
}

impl<A: OneShotGetAddressTxids> OneShotGetAddressTxids for Quorum<A> {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        first!(self, get_address_txids, addresses, start, end)
    }
}

impl<A: OneShotGetAddressDeltas> OneShotGetAddressDeltas for Quorum<A> {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        first!(self, get_address_deltas, addresses, start, end)
    }
}

impl<A: OneShotGetChainTips> OneShotGetChainTips for Quorum<A> {
    async fn get_chain_tips(&self) -> Result<Vec<rpc::ChainTip>, QueryError<GetChainTipsError>> {
        // One member's view of the block tree; the tips of several are not
        // one tree.
        first!(self, get_chain_tips)
    }
}

impl<A: OneShotGetDifficulty> OneShotGetDifficulty for Quorum<A> {
    async fn get_difficulty(&self) -> Result<Difficulty, QueryError<GetDifficultyError>> {
        first!(self, get_difficulty)
    }
}

impl<A: OneShotGetBlockchainInfo> OneShotGetBlockchainInfo for Quorum<A> {
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        first!(self, get_blockchain_info)
    }
}

impl<A: OneShotGetNodeInfo> OneShotGetNodeInfo for Quorum<A> {
    async fn get_node_info(&self) -> Result<rpc::NodeInfo, QueryError<GetNodeInfoError>> {
        first!(self, get_node_info)
    }
}

impl<A: OneShotGetPeerInfo> OneShotGetPeerInfo for Quorum<A> {
    async fn get_peer_info(&self) -> Result<Vec<rpc::PeerInfo>, QueryError<GetPeerInfoError>> {
        first!(self, get_peer_info)
    }
}

impl<A: OneShotGetMiningInfo> OneShotGetMiningInfo for Quorum<A> {
    async fn get_mining_info(&self) -> Result<rpc::MiningInfo, QueryError<GetMiningInfoError>> {
        first!(self, get_mining_info)
    }
}

impl<A: OneShotGetNetworkSolPs> OneShotGetNetworkSolPs for Quorum<A> {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<GetNetworkSolPsError>> {
        first!(self, get_network_sol_ps, blocks, height)
    }
}

// ---------------------------------------------------------------------------
// The mempool: one member at a time
// ---------------------------------------------------------------------------

impl<A: OneShotGetMempoolTxids> OneShotGetMempoolTxids for Quorum<A> {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        pinned!(self, get_mempool_txids)
    }
}

impl<A: OneShotGetMempoolMetadata> OneShotGetMempoolMetadata for Quorum<A> {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        pinned!(self, get_mempool_metadata)
    }
}

impl<A: OneShotGetRawMempoolTransaction> OneShotGetRawMempoolTransaction for Quorum<A> {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        pinned!(self, get_raw_mempool_transaction, txid)
    }
}

impl<A: OneShotGetMempoolCompactTransaction> OneShotGetMempoolCompactTransaction for Quorum<A> {
    async fn get_mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<PreIndexCompactTx, QueryError<GetRawMempoolTransactionError>> {
        pinned!(self, get_mempool_compact_transaction, txid)
    }
}

impl<A: OneShotGetMempoolSourceTip> OneShotGetMempoolSourceTip for Quorum<A> {
    async fn get_mempool_source_tip(&self) -> Result<(BlockHash, Height), QueryError<Infallible>> {
        // The tip of the member whose mempool is being served, by the port's
        // single-source rule — not the agreed tip, which may be another
        // member's.
        pinned!(self, get_mempool_source_tip)
    }
}
