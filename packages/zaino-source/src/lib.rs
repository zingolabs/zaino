//! Zaino source — driven port traits for validator access.
//!
//! One trait per question a consumer can ask about the chain.
//! Implementations (adapters) bridge to a specific transport
//! (JSON-RPC, Zebra ReadState, mock).
//!
//! Consumers compose traits via bounds:
//! ```ignore
//! fn sync<V: GetBlock + GetChainTip>(validator: &V) { ... }
//! ```

mod error;
mod get_address_balance;
mod get_address_deltas;
mod get_address_txids;
mod get_address_utxos;
mod get_best_block_height;
mod get_block;
mod get_block_by_hash;
mod get_block_deltas;
mod get_block_header;
mod get_block_subsidy;
mod get_block_verbose;
mod get_blockchain_info;
mod get_chain_tip;
mod get_chain_tips;
mod get_commitment_tree_roots;
mod get_compact_block;
mod get_difficulty;
mod get_mempool_metadata;
mod get_mempool_source_tip;
mod get_mempool_txids;
mod get_mining_info;
mod get_network_sol_ps;
mod get_node_info;
mod get_peer_info;
mod get_raw_block;
mod get_raw_block_header;
mod get_raw_mempool_transaction;
mod get_spent_info;
mod get_subtree_roots;
mod get_transaction;
mod get_treestate;
mod get_treestate_by_hash;
mod get_tx_out;
mod lifecycle;
mod polled_chain_tip;
mod send_raw_transaction;
mod subscribe_blocks;
mod subscribe_chain_tip;

pub mod resilient;

pub use error::{FailureMode, FetchError, QueryError, SourceError, UnavailableError};
pub use get_address_balance::{GetAddressBalanceError, OneShotGetAddressBalance};
pub use get_address_deltas::{GetAddressDeltasError, OneShotGetAddressDeltas};
pub use get_address_txids::{GetAddressTxidsError, OneShotGetAddressTxids};
pub use get_address_utxos::{GetAddressUtxosError, OneShotGetAddressUtxos};
pub use get_best_block_height::{GetBestBlockHeightError, OneShotGetBestBlockHeight};
pub use get_block::{GetBlockError, OneShotGetBlock};
pub use get_block_by_hash::{GetBlockByHashError, OneShotGetBlockByHash};
pub use get_block_deltas::{GetBlockDeltasError, OneShotGetBlockDeltas};
pub use get_block_header::{GetBlockHeaderError, OneShotGetBlockHeader};
pub use get_block_subsidy::{GetBlockSubsidyError, OneShotGetBlockSubsidy};
pub use get_block_verbose::{GetBlockVerboseByHash, GetBlockVerboseError, OneShotGetBlockVerbose};
pub use get_blockchain_info::{GetBlockchainInfoError, OneShotGetBlockchainInfo};
pub use get_chain_tip::{GetChainTipError, OneShotGetChainTip};
pub use get_chain_tips::{GetChainTipsError, OneShotGetChainTips};
pub use get_commitment_tree_roots::{GetCommitmentTreeRootsError, OneShotGetCommitmentTreeRoots};
pub use get_compact_block::OneShotGetPreIndexCompactBlock;
pub use get_difficulty::{GetDifficultyError, OneShotGetDifficulty};
pub use get_mempool_metadata::{GetMempoolMetadataError, MempoolTxMeta, OneShotGetMempoolMetadata};
pub use get_mempool_source_tip::OneShotGetMempoolSourceTip;
pub use get_mempool_txids::{GetMempoolTxidsError, OneShotGetMempoolTxids};
pub use get_mining_info::{GetMiningInfoError, OneShotGetMiningInfo};
pub use get_network_sol_ps::{GetNetworkSolPsError, OneShotGetNetworkSolPs};
pub use get_node_info::{GetNodeInfoError, OneShotGetNodeInfo};
pub use get_peer_info::{GetPeerInfoError, OneShotGetPeerInfo};
pub use get_raw_block::{OneShotGetRawBlock, OneShotGetRawBlockByHash};
pub use get_raw_block_header::OneShotGetRawBlockHeader;
pub use get_raw_mempool_transaction::{
    GetRawMempoolTransactionError, OneShotGetRawMempoolTransaction,
};
pub use get_spent_info::{GetSpentInfoError, OneShotGetSpentInfo};
pub use get_subtree_roots::{GetSubtreeRootsError, OneShotGetSubtreeRoots};
pub use get_transaction::{GetTransactionError, OneShotGetTransaction, TransactionResponse};
pub use get_treestate::{GetTreestate, GetTreestateError};
pub use get_treestate_by_hash::{GetTreestateByHash, GetTreestateByHashError};
pub use get_tx_out::{GetTxOut, GetTxOutError};
pub use lifecycle::SourceLifecycle;
pub use polled_chain_tip::PolledChainTip;
pub use resilient::{Resilient, RetryPolicy};
pub use send_raw_transaction::{SendRawTransaction, SendRawTransactionError};
pub use subscribe_blocks::SubscribeBlocks;
pub use subscribe_chain_tip::{SubscribeChainTip, TipObservation};

// `cfg(test)` as well as the feature: without it this crate's own tests never
// compile the mock, so neither its tests nor `Resilient`'s integration tests
// run in a bare `cargo test` — nothing in the workspace enables `testing`, and
// the gap is invisible because a module that is not compiled reports no
// failures. The feature stays so downstream crates can opt in; `cfg(test)`
// makes the mock's own coverage unconditional.
#[cfg(any(test, feature = "testing"))]
pub mod mock;
