//! Zaino's validator source: the questions Zaino asks a validator, answered over JSON-RPC
//!
//! - [`queries`](crate::GetBlock): one trait per question, each with its own domain error
//! - [`ZebraRpcAdapter`]: the one implementation (blocks decoded once, from consensus bytes)
//! - [`BlockFetchPool`]: blocks from N validators, ordered, concurrent, retried

mod adapter;
mod balance;
mod decode;
mod error;
mod fetch_pool;
mod indexer;
mod parse;
mod queries;
mod rpc;

pub use adapter::ZebraRpcAdapter;
pub use balance::TrafficBalancer;
pub use decode::{decode_transaction, prepare_transaction, DecodeError, Prepared};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use fetch_pool::BlockFetchPool;
pub use indexer::{Change, IndexerWatch};
pub use queries::{
    BlockLink, BlockLinks, ChainDataSource, GetBlockByHashError, GetBlockError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, PollReading, RawMempoolTransactions, SendRawTransactionError,
    TransactionResponse,
};
pub use rpc::{describe_metrics, METRIC_BUCKETS};
pub use rpc::{EndpointError, Lane, LinkLimits, RpcClient, RpcClientConfig, RpcError, Timeouts};

/// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod mock;
