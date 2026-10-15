//! Zaino's validator source: the questions Zaino asks a validator
//!
//! - [`ChainDataSource`]: one port, each question with its own domain error, transport-agnostic
//! - [`ZcashLegacyRpcClient`]: every question over JSON-RPC (zebrad or zcashd)
//! - [`ZebraIndexerGrpcClient`]: blocks over zebrad's indexer gRPC, the rest over JSON-RPC
//! - Both decode blocks once, from consensus bytes; which validator answers = `zaino-traffic`'s

mod blockchain_info;
mod decode;
mod error;
mod grpc_client;
mod legacy_rpc;
mod node_release;
mod parse;
mod peer_info;
mod queries;
mod rpc;
mod standard_fee;
mod zebra_indexer;

pub use blockchain_info::{BlockchainInfo, ScheduleError};
pub use decode::{decode_block, decode_transaction, prepare_transaction, DecodeError, Prepared};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use grpc_client::ZebraIndexerGrpcClient;
pub use legacy_rpc::ZcashLegacyRpcClient;
pub use node_release::{EndOfService, NodeRelease};
pub use peer_info::PeerInfo;
pub use queries::{
    BlockLink, BlockLinks, ChainDataSource, CheckAdmissionError, GetAtHeightError,
    GetBlockByHashError, GetMempoolListingError, GetRawMempoolTransactionError,
    GetStandardFeeError, GetTransactionError, MempoolListed, MetadataReading, PollReading,
    RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};
pub use rpc::{describe_metrics, METRIC_BUCKETS};
pub use rpc::{EndpointError, LinkLimits, ResponseSizeCap, RpcError, Timeouts};
pub use standard_fee::StandardFee;
pub use zebra_indexer::{ZebraIndexerTls, ZebraIndexerWatch};

// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod testing;
