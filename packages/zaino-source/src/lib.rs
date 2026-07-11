//! Zaino source — driven port traits for validator access.
//!
//! One trait per question a consumer can ask about the chain.
//! Implementations (adapters) bridge to a specific transport
//! (JSON-RPC, Zebra ReadState, mock).
//!
//! Consumers compose traits via bounds:
//! ```ignore
//! fn sync<V: GetBlockBytes + GetChainTip>(validator: &V) { ... }
//! ```

mod error;
mod get_address_balance;
mod get_address_deltas;
mod get_address_txids;
mod get_address_utxos;
mod get_best_block_height;
mod get_block_by_hash;
mod get_block_bytes;
mod get_block_verbose;
mod get_chain_tip;
mod get_commitment_tree_roots;
mod get_mempool_txids;
mod get_subtree_roots;
mod get_transaction;
mod get_treestate;

pub use error::{QueryError, TransportError};
pub use get_address_balance::{GetAddressBalance, GetAddressBalanceError};
pub use get_address_deltas::{GetAddressDeltas, GetAddressDeltasError};
pub use get_address_txids::{GetAddressTxids, GetAddressTxidsError};
pub use get_address_utxos::{GetAddressUtxos, GetAddressUtxosError};
pub use get_best_block_height::{GetBestBlockHeight, GetBestBlockHeightError};
pub use get_block_by_hash::{GetBlockByHash, GetBlockByHashError};
pub use get_block_bytes::{GetBlockBytes, GetBlockBytesError};
pub use get_block_verbose::{GetBlockVerbose, GetBlockVerboseError};
pub use get_chain_tip::{GetChainTip, GetChainTipError};
pub use get_commitment_tree_roots::{GetCommitmentTreeRoots, GetCommitmentTreeRootsError};
pub use get_mempool_txids::{GetMempoolTxids, GetMempoolTxidsError};
pub use get_subtree_roots::{GetSubtreeRoots, GetSubtreeRootsError};
pub use get_transaction::{GetTransaction, GetTransactionError, TransactionResponse};
pub use get_treestate::{GetTreestate, GetTreestateError};
