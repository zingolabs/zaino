//! Zaino's validator source: the questions Zaino asks a validator
//!
//! - [`ChainDataSource`]: one port, each question with its own domain error, transport-agnostic
//! - [`ZcashLegacyRpcClient`]: every question over JSON-RPC (zebrad or zcashd)
//! - [`ZcashGrpcClient`]: blocks over zebrad's indexer gRPC, the rest over JSON-RPC
//! - Both decode blocks once, from consensus bytes; which validator answers = `zaino-traffic`'s

mod decode;
mod error;
mod grpc_client;
mod indexer;
mod legacy_rpc;
mod parse;
mod queries;
mod rpc;

pub use decode::{decode_block, decode_transaction, prepare_transaction, DecodeError, Prepared};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use grpc_client::ZcashGrpcClient;
pub use indexer::{Change, IndexerTls, IndexerWatch};
pub use legacy_rpc::ZcashLegacyRpcClient;
pub use queries::{
    BlockLink, BlockLinks, ChainDataSource, CheckAdmissionError, GetAtHeightError,
    GetBlockByHashError, GetMempoolListingError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, MetadataReading, PollReading, RawMempoolTransactions,
    SendRawTransactionError, TransactionResponse,
};
pub use rpc::{describe_metrics, METRIC_BUCKETS};
pub use rpc::{EndpointError, LinkLimits, RpcClient, RpcClientConfig, RpcError, Timeouts};

// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod testing;
