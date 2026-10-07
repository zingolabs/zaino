//! Zaino's validator source: the questions Zaino asks a validator, answered over JSON-RPC
//!
//! - [`ChainDataSource`]: one port, each question with its own domain error
//! - [`ZebraRpcAdapter`]: the one implementation (blocks decoded once, from consensus bytes)
//! - Which validator answers = `zaino-traffic`'s

mod adapter;
mod decode;
mod error;
mod indexer;
mod parse;
mod queries;
mod rpc;

pub use adapter::ZebraRpcAdapter;
pub use decode::{decode_transaction, prepare_transaction, DecodeError, Prepared};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use indexer::{Change, IndexerWatch};
pub use queries::{
    BlockLink, BlockLinks, ChainDataSource, GetAtHeightError, GetBlockByHashError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, PollReading, RawMempoolTransactions, SendRawTransactionError,
    TransactionResponse,
};
pub use rpc::{describe_metrics, METRIC_BUCKETS};
pub use rpc::{EndpointError, LinkLimits, RpcClient, RpcClientConfig, RpcError, Timeouts};

/// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod mock;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
