//! Zaino's validator source: the questions Zaino asks a validator
//!
//! - [`ChainDataSource`]: one port, each question with its own domain error, transport-agnostic
//! - [`ZcashLegacyRpcClient`]: every question over JSON-RPC (zebrad or zcashd)
//! - [`ZebraIndexerGrpcClient`]: blocks over zebrad's indexer gRPC, the rest over JSON-RPC
//! - Both decode blocks once, from consensus bytes
//! - Which validator answers = `zaino-traffic`'s

mod blockchain_info;
mod chain_data_source;
mod decode;
mod endpoint;
mod error;
mod indexer_client;
mod indexer_endpoint;
mod indexer_watch;
mod json;
mod legacy_rpc;
mod metrics;
mod node_release;
mod peer_info;
mod rpc;

pub use self::metrics::METRICS;
pub use blockchain_info::{BlockchainInfo, ScheduleError};
pub use chain_data_source::{
    BlockLink, BlockLinks, ChainDataSource, GetAtHeightError, GetBlockByHashError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, PollReading, RawMempoolTransactions, SendRawTransactionError, Telemetry,
    TransactionResponse,
};
pub use decode::{
    decode_block, decode_prepared, decode_transaction, mined_transaction, prepare_transaction,
    DecodeError, Prepared,
};
pub use endpoint::{Auth, Endpoint, EndpointError, LinkLimits, Timeouts};
pub use error::{FailureMode, NonDomainError, QueryError};
pub use indexer_client::ZebraIndexerGrpcClient;
pub use indexer_endpoint::ZebraIndexerTls;
pub use indexer_watch::ZebraIndexerWatch;
pub use legacy_rpc::ZcashLegacyRpcClient;
pub use node_release::{EndOfService, NodeRelease};
pub use peer_info::PeerInfo;
pub use rpc::{ResponseSizeCap, RpcError};

// `cfg(test)` too (a crate's own features don't self-enable)
#[cfg(any(test, feature = "testing"))]
pub mod testing;
