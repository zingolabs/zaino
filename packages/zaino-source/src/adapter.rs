//! The ports over zebrad's JSON-RPC

use std::convert::Infallible;
use std::path::Path;

use zaino_primitives::types::PeerInfo;
use zaino_primitives::types::{Block, BlockHash, BlockchainInfo, Height, TransactionId};

use crate::rpc::{
    auth_from_parts, validator_url, EndpointError, RpcClient, RpcClientConfig, RpcError, Timeouts,
};
use crate::{
    decode, parse, BlockLink, FailureMode, GetBlockByHashError, GetBlockError, GetBlockLinkError,
    GetChainTipError, GetMempoolListingError, GetPeerInfoError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, NonDomainError, QueryError, SendRawTransactionError,
    TransactionResponse,
};

/// Single attempt per call (callers own their retry)
pub struct ZebraRpcAdapter {
    rpc: RpcClient,
}

impl ZebraRpcAdapter {
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }

    /// The validator at `address` (`host:port`), unprobed: whether it answers is the first call's
    /// question, so a validator down at boot is the caller's retry, never a boot failure
    pub fn at(
        address: &str,
        cookie_path: Option<&Path>,
        user: Option<String>,
        password: Option<String>,
        timeouts: Timeouts,
    ) -> Result<Self, EndpointError> {
        let rpc = RpcClient::new(RpcClientConfig {
            url: validator_url(address)?,
            auth: auth_from_parts(cookie_path, user, password)?,
            timeouts,
            ..RpcClientConfig::default()
        })
        .map_err(EndpointError::Client)?;
        Ok(Self::new(rpc))
    }

    /// `getblock <id> 0` → bytes (const-hex) → [`decode::block`]
    async fn raw_block<E>(
        &self,
        id: String,
        absent: impl FnOnce() -> E,
    ) -> Result<Block, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let params = vec![id.into(), serde_json::Value::Number(0.into())];
        let parse::HexBytes(raw) = self
            .rpc
            .call_as("getblock", params)
            .await
            .map_err(|error| absent_or_fetch(error, absent))?;
        decode::block(&raw).map_err(|e| NonDomainError::from_cause(FailureMode::Parse, e).into())
    }

    /// Call + parse; `classify` decides which transport failures are the port's answer
    async fn call_parsed<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
        classify: impl FnOnce(RpcError) -> QueryError<E>,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let value = self.rpc.call(method, params).await.map_err(classify)?;
        parse(&value).map_err(|e| QueryError::NonDomain(from_parse(e)))
    }
}

fn from_parse(e: parse::ParseError) -> NonDomainError {
    NonDomainError::from_cause(FailureMode::Parse, e)
}

fn fetch_failure<E: std::fmt::Debug + std::fmt::Display>(error: RpcError) -> QueryError<E> {
    QueryError::NonDomain(error.into())
}

/// zebrad's "no such object": `-8` on `getblock <height>`, `-5` on hash/txid lookups
///
/// - Both also mean "malformed parameter" upstream; safe here (every param rendered from a
///   domain type, so none can be malformed)
const NOT_FOUND_CODES: [i64; 2] = [-5, -8];

/// JSON-RPC "method not implemented"
const METHOD_NOT_FOUND: i64 = -32601;

/// Not-found code → the port's absent answer; anything else stays a fetch failure
fn absent_or_fetch<E>(error: RpcError, absent: impl FnOnce() -> E) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    let error: NonDomainError = error.into();
    match error.mode {
        FailureMode::RpcError(code) if NOT_FOUND_CODES.contains(&code) => {
            QueryError::Domain(absent())
        }
        _ => QueryError::NonDomain(error),
    }
}

/// `-22` unparseable, `-25` to `-27` (both inclusive) declined: answers about the transaction,
/// reason kept
fn submission_rejection(error: &NonDomainError) -> Option<SendRawTransactionError> {
    match error.mode {
        FailureMode::RpcError(-22) => {
            Some(SendRawTransactionError::Malformed(error.message.clone()))
        }
        FailureMode::RpcError(-27..=-25) => {
            Some(SendRawTransactionError::Rejected(error.message.clone()))
        }
        _ => None,
    }
}

/// zebrad's `-1` on `getrawmempool true` below the network tip
///
/// - `-1` = zebra's catch-all `Misc`, so the message is the only discriminant
/// - zebrad/src/components/mempool.rs (`Request::FullTransactions` while disabled)
const MEMPOOL_INACTIVE: &str = "mempool is not active";

/// `getrawmempool` answers about the node, not transient failures:
/// `-32601` = no mempool at all, `-1` + [`MEMPOOL_INACTIVE`] = none until caught up
fn mempool_unavailable_or_fetch(error: RpcError) -> QueryError<GetMempoolListingError> {
    let error: NonDomainError = error.into();
    match error.mode {
        FailureMode::RpcError(METHOD_NOT_FOUND) => {
            QueryError::Domain(GetMempoolListingError::Unavailable)
        }
        FailureMode::RpcError(-1) if error.message.starts_with(MEMPOOL_INACTIVE) => {
            QueryError::Domain(GetMempoolListingError::Inactive)
        }
        _ => QueryError::NonDomain(error),
    }
}

/// RPC display order (byte-reversed hex)
fn display_hex(mut bytes: [u8; 32]) -> String {
    bytes.reverse();
    const_hex::encode(bytes)
}

impl crate::GetBlock for ZebraRpcAdapter {
    #[tracing::instrument(skip(self), fields(h = u32::from(height)))]
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.raw_block(u32::from(height).to_string(), || GetBlockError::HeightNotFound(height))
            .await
    }
}

impl crate::GetBlockByHash for ZebraRpcAdapter {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.raw_block(display_hex(hash.into()), || GetBlockByHashError::NotFound(hash)).await
    }
}

impl crate::GetBlockLink for ZebraRpcAdapter {
    /// Raw form (verbose = two extra state reads per header on zebrad)
    async fn get_block_link(
        &self,
        height: Height,
    ) -> Result<BlockLink, QueryError<GetBlockLinkError>> {
        let params = vec![u32::from(height).to_string().into(), serde_json::Value::Bool(false)];
        let parse::HexBytes(raw) =
            self.rpc.call_as("getblockheader", params).await.map_err(|error| {
                absent_or_fetch(error, || GetBlockLinkError::HeightNotFound(height))
            })?;
        decode::block_link(&raw)
            .map_err(|e| NonDomainError::from_cause(FailureMode::Parse, e).into())
    }
}

impl crate::GetChainTip for ZebraRpcAdapter {
    #[tracing::instrument(skip(self))]
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        self.call_parsed("getbestblockheightandhash", vec![], parse::parse_best_tip, fetch_failure)
            .await
    }
}

impl crate::GetBlockchainInfo for ZebraRpcAdapter {
    async fn get_blockchain_info(&self) -> Result<BlockchainInfo, QueryError<Infallible>> {
        self.call_parsed("getblockchaininfo", vec![], parse::parse_blockchain_info, fetch_failure)
            .await
    }
}

impl crate::GetMempoolListing for ZebraRpcAdapter {
    async fn get_mempool_listing(
        &self,
    ) -> Result<Vec<MempoolListed>, QueryError<GetMempoolListingError>> {
        self.call_parsed(
            "getrawmempool",
            vec![serde_json::Value::Bool(true)],
            parse::parse_mempool_listing,
            mempool_unavailable_or_fetch,
        )
        .await
    }
}

impl crate::GetRawMempoolTransaction for ZebraRpcAdapter {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        let params = vec![display_hex(txid.into()).into(), serde_json::Value::Number(0.into())];
        self.call_parsed("getrawtransaction", params, parse::parse_raw_transaction, |error| {
            absent_or_fetch(error, || GetRawMempoolTransactionError::NotFound(txid))
        })
        .await
    }
}

impl crate::SendRawTransaction for ZebraRpcAdapter {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        let params = vec![const_hex::encode(transaction).into()];
        self.call_parsed("sendrawtransaction", params, parse::as_txid, |error| {
            let error: NonDomainError = error.into();
            match submission_rejection(&error) {
                Some(rejection) => QueryError::Domain(rejection),
                None => QueryError::NonDomain(error),
            }
        })
        .await
    }
}

impl crate::GetPeerInfo for ZebraRpcAdapter {
    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>> {
        self.call_parsed("getpeerinfo", vec![], parse::parse_peer_info, fetch_failure).await
    }
}

impl crate::GetTransaction for ZebraRpcAdapter {
    /// Verbosity 1: the height places it (mined vs mempool)
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        let params = vec![display_hex(txid.into()).into(), serde_json::Value::Number(1.into())];
        self.call_parsed("getrawtransaction", params, parse::parse_transaction, |error| {
            absent_or_fetch(error, || GetTransactionError::NotFound(txid))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(code: i64, message: &str) -> RpcError {
        RpcError::Rpc { code, message: message.to_string() }
    }

    /// Both not-found codes = the port's absent answer (a missing block misfiled as a failure
    /// stalls sync against a healthy validator); every other code and every transport failure
    /// stays a failure (an outage must never read as an empty chain)
    #[test]
    fn only_not_found_codes_are_absent_answers() {
        let absent = || GetBlockError::HeightNotFound(Height::try_from(42u32).expect("h"));
        for code in NOT_FOUND_CODES {
            let classified = absent_or_fetch(rpc(code, "not found"), absent);
            assert!(matches!(classified, QueryError::Domain(_)), "code {code}: {classified:?}");
        }
        for error in [-1, -3, -20, -22, -25, -28, -32_600]
            .map(|code| rpc(code, "something else"))
            .into_iter()
            .chain([RpcError::Status(503)])
        {
            let classified = absent_or_fetch(error, absent);
            assert!(matches!(classified, QueryError::NonDomain(_)), "{classified:?}");
        }
    }

    /// Rejections carry the reason (the only useful part); a warming-up node has not
    /// considered the transaction at all
    #[test]
    fn submission_rejections_carry_their_reason() {
        use SendRawTransactionError::{Malformed, Rejected};
        let malformed = submission_rejection(&rpc(-22, "tx unparseable").into());
        assert!(matches!(malformed, Some(Malformed(reason)) if reason == "tx unparseable"));
        for code in [-25, -26, -27] {
            let rejection = submission_rejection(&rpc(code, "rejected").into());
            assert!(matches!(rejection, Some(Rejected(_))), "code {code}");
        }
        assert!(submission_rejection(&rpc(-28, "warming up").into()).is_none());
    }

    /// `-32601` = no mempool on this node (stop asking); zebrad's own `-1` below the network tip
    /// = inactive; every other code, a bare `-1` included, stays a failure worth re-polling
    #[test]
    fn mempool_answers_are_a_missing_method_or_an_inactive_mempool() {
        let missing = mempool_unavailable_or_fetch(rpc(METHOD_NOT_FOUND, "Method not found"));
        assert!(matches!(missing, QueryError::Domain(GetMempoolListingError::Unavailable)));
        let inactive = mempool_unavailable_or_fetch(rpc(
            -1,
            "mempool is not active: wait for Zebra to sync to the tip",
        ));
        assert!(matches!(inactive, QueryError::Domain(GetMempoolListingError::Inactive)));
        for code in [-5, -8, -28, -1, -32603] {
            let classified = mempool_unavailable_or_fetch(rpc(code, "something else"));
            assert!(matches!(classified, QueryError::NonDomain(_)), "code {code}");
        }
    }
}
