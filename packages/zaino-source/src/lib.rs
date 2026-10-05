//! Zaino's validator source: the questions Zaino asks a validator, answered over JSON-RPC
//!
//! - [`queries`](crate::GetBlock): one trait per question, each with its own domain error
//! - [`ZebraRpcAdapter`]: the one implementation (blocks decoded once, from consensus bytes)
//! - [`BlockFetchPool`]: blocks from N validators, ordered, concurrent, retried

mod adapter;
mod decode;
mod error;
mod fetch_pool;
mod parse;
mod queries;
mod rpc;

pub use adapter::ZebraRpcAdapter;
pub use decode::{decode_transaction, DecodeError};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use fetch_pool::{BlockFetchPool, FetchRoute};
pub use queries::{
    BlockLink, GetBlock, GetBlockByHash, GetBlockByHashError, GetBlockError, GetBlockLink,
    GetBlockLinkError, GetBlockchainInfo, GetBlockchainInfoError, GetChainTip, GetChainTipError,
    GetMempoolListing, GetMempoolListingError, GetMempoolSourceTip, GetPeerInfo, GetPeerInfoError,
    GetRawMempoolTransaction, GetRawMempoolTransactionError, GetTransaction, GetTransactionError,
    MempoolListed, SendRawTransaction, SendRawTransactionError, SourceTip, TransactionResponse,
};
pub use rpc::{describe_metrics, METRIC_BUCKETS};
pub use rpc::{ProbeError, RpcClient, RpcClientConfig, RpcError, Timeouts};

/// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod mock;
