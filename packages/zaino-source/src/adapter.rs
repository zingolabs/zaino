//! The ports over zebrad's JSON-RPC

use std::path::Path;

use zaino_primitives::types::{Block, BlockHash, Height, TransactionId};

use crate::rpc::{
    auth_from_parts, validator_url, Call, EndpointError, LinkLimits, RpcClient, RpcClientConfig,
    RpcError, Timeouts,
};
use crate::{
    decode, parse, BlockLink, BlockLinks, FailureMode, GetAtHeightError, GetBlockByHashError,
    GetMempoolListingError, GetRawMempoolTransactionError, GetTransactionError, MempoolListed,
    MetadataReading, NonDomainError, PollReading, QueryError, RawMempoolTransactions,
    SendRawTransactionError, TransactionResponse,
};

/// One validator's link; single attempt per call (retries, concurrency = the caller's balancer)
pub struct ZebraRpcAdapter {
    rpc: RpcClient,
}

impl ZebraRpcAdapter {
    /// Validator at `address` (`host:port`), unprobed (down at boot = the first call's failure +
    /// the caller's retry, never a boot failure)
    pub fn at(
        address: &str,
        cookie_path: Option<&Path>,
        user: Option<String>,
        password: Option<String>,
        timeouts: Timeouts,
        limits: LinkLimits,
    ) -> Result<Self, EndpointError> {
        let rpc = RpcClient::new(RpcClientConfig {
            url: validator_url(address)?,
            name: address.to_owned(),
            auth: auth_from_parts(cookie_path, user, password)?,
            timeouts,
            limits,
        })
        .map_err(EndpointError::Client)?;
        Ok(Self { rpc })
    }

    /// `getblock <id> 0` → bytes → [`decode::block`]
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

/// zebrad's "no such object": `-8` on `getblock <height>`, `-5` on hash/txid lookups
///
/// - both also "malformed parameter" upstream (safe: every param rendered from a domain type)
const NOT_FOUND_CODES: [i64; 2] = [-5, -8];

/// `getblockhash` above the tip: zebrad `-32602` (index past the tip), zcashd `-8` (out of range)
const ABOVE_TIP_CODES: [i64; 2] = [-8, -32602];

/// JSON-RPC "method not implemented"
const METHOD_NOT_FOUND: i64 = -32601;

/// Not-found code → the port's absent answer; anything else stays a fetch failure
fn absent_or_fetch<E>(error: RpcError, absent: impl FnOnce() -> E) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    absent_on(&NOT_FOUND_CODES, error, absent)
}

/// One of `codes` → the port's absent answer; anything else stays a fetch failure
fn absent_on<E>(codes: &[i64], error: RpcError, absent: impl FnOnce() -> E) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    let error: NonDomainError = error.into();
    match error.mode {
        FailureMode::RpcError(code) if codes.contains(&code) => QueryError::Domain(absent()),
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
/// - `-1` = zebra's catch-all `Misc` (message = only discriminant)
/// - zebrad/src/components/mempool.rs (`Request::FullTransactions` while disabled)
const MEMPOOL_INACTIVE: &str = "mempool is not active";

/// `getrawmempool` answers about the node, not transient failures
///
/// - `-32601` = no mempool at all; `-1` + [`MEMPOOL_INACTIVE`] = none until caught up
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

/// Headers per `getblockheader` batch (~3 KB hex each: ~1.5 MB reply)
const LINK_BATCH_CALLS: usize = 500;

impl crate::ChainDataSource for ZebraRpcAdapter {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.raw_block(hash.to_string(), || GetBlockByHashError::NotFound(hash)).await
    }

    /// Raw form (verbose = two extra state reads per header on zebrad)
    async fn get_block_links(&self, heights: &[Height]) -> Result<BlockLinks, NonDomainError> {
        let mut links = Vec::with_capacity(heights.len());
        for batch in heights.chunks(LINK_BATCH_CALLS) {
            let calls = batch
                .iter()
                .map(|height| Call {
                    method: "getblockheader",
                    params: vec![u32::from(*height).to_string().into(), false.into()],
                })
                .collect();
            let replies = self.rpc.call_batch::<parse::HexBytes>(calls).await?;
            for (height, reply) in batch.iter().zip(replies) {
                let absent = || GetAtHeightError::HeightNotFound(*height);
                links.push(match split(reply.map_err(|error| absent_or_fetch(error, absent)))? {
                    Ok(parse::HexBytes(header)) => Ok(BlockLink { header }),
                    Err(gone) => Err(gone),
                });
            }
        }
        Ok(links)
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        let call = |method, params| Call { method, params };
        let mut calls =
            vec![call("getblockchaininfo", vec![]), call("getrawmempool", vec![true.into()])];
        calls.extend(holds.iter().map(|h| call("getblockhash", vec![u32::from(*h).into()])));
        if metadata {
            calls.extend(["getpeerinfo", "getinfo", "getdeprecationinfo"].map(|m| call(m, vec![])));
        }
        let mut replies = self.rpc.call_batch::<serde_json::Value>(calls).await?.into_iter();
        let mut next = || replies.next().expect("a batch answers every call (parse_batch)");

        let info = parse::parse_blockchain_info(&next()?).map_err(from_parse)?;
        let listing = match split(next().map_err(mempool_unavailable_or_fetch))? {
            Ok(listing) => Ok(parse::parse_mempool_listing(&listing).map_err(from_parse)?),
            Err(refused) => Err(refused),
        };
        let held = holds
            .iter()
            .map(|height| {
                let absent = || GetAtHeightError::HeightNotFound(*height);
                let hash = next().map_err(|error| absent_on(&ABOVE_TIP_CODES, error, absent))?;
                parse::as_block_hash(&hash).map_err(|e| QueryError::NonDomain(from_parse(e)))
            })
            .collect();
        let metadata = metadata.then(|| {
            let peers = next().map_err(NonDomainError::from);
            let info = next().map_err(NonDomainError::from);
            let deprecation = match next() {
                Ok(value) => Ok(Some(value)),
                Err(RpcError::Rpc { code: METHOD_NOT_FOUND, .. }) => Ok(None),
                Err(other) => Err(NonDomainError::from(other)),
            };
            MetadataReading {
                peers: peers.and_then(|peers| parse::parse_peer_info(&peers).map_err(from_parse)),
                release: info.and_then(|info| {
                    let deprecation = deprecation?;
                    parse::parse_node_release(&info, deprecation.as_ref()).map_err(from_parse)
                }),
            }
        });
        Ok(PollReading { info, listing, held, metadata })
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        let mut fetched = Vec::with_capacity(listed.len());
        for batch in raw_batches(listed) {
            let calls = batch
                .iter()
                .map(|entry| Call {
                    method: "getrawtransaction",
                    params: vec![entry.txid.to_string().into(), 0.into()],
                })
                .collect();
            let replies = self.rpc.call_batch(calls).await?;
            for (entry, reply) in batch.iter().zip(replies) {
                let absent = || GetRawMempoolTransactionError::NotFound(entry.txid);
                fetched.push(match split(reply.map_err(|error| absent_or_fetch(error, absent)))? {
                    Ok(value) => Ok(parse::parse_raw_transaction(&value).map_err(from_parse)?),
                    Err(gone) => Err(gone),
                });
            }
        }
        Ok(fetched)
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        let params = vec![hex::encode(transaction).into()];
        self.call_parsed("sendrawtransaction", params, parse::as_txid, |error| {
            let error: NonDomainError = error.into();
            match submission_rejection(&error) {
                Some(rejection) => QueryError::Domain(rejection),
                None => QueryError::NonDomain(error),
            }
        })
        .await
    }

    /// Verbosity 1: the height places it (mined vs mempool)
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        let params = vec![txid.to_string().into(), serde_json::Value::Number(1.into())];
        self.call_parsed("getrawtransaction", params, parse::parse_transaction, |error| {
            absent_or_fetch(error, || GetTransactionError::NotFound(txid))
        })
        .await
    }
}

/// Port's answer apart from its failure: `Ok(Err(domain))` = the validator's answer
fn split<T, E>(result: Result<T, QueryError<E>>) -> Result<Result<T, E>, NonDomainError>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(QueryError::Domain(answer)) => Ok(Err(answer)),
        Err(QueryError::NonDomain(cause)) => Err(cause),
    }
}

/// Raw bytes per `getrawtransaction` batch (hex doubles it: 16 MiB reply, under
/// `MAX_RESPONSE_BYTES` and zebra's 50 MiB `max_response_body_size` default)
const RAW_BATCH_BYTES: u64 = 8 << 20;
/// Calls per batch (one batch holds one connection for its whole reply)
const RAW_BATCH_CALLS: usize = 100;

/// `listed` cut in order into batches within both budgets (an entry over the byte budget alone)
fn raw_batches(listed: &[MempoolListed]) -> Vec<&[MempoolListed]> {
    let mut batches = Vec::new();
    let (mut start, mut bytes) = (0, 0u64);
    for (index, entry) in listed.iter().enumerate() {
        let len = u64::from(entry.encoded_len);
        let full = index - start == RAW_BATCH_CALLS || bytes + len > RAW_BATCH_BYTES;
        if full && index > start {
            batches.push(&listed[start..index]);
            (start, bytes) = (index, 0);
        }
        bytes += len;
    }
    if start < listed.len() {
        batches.push(&listed[start..]);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(code: i64, message: &str) -> RpcError {
        RpcError::Rpc { code, message: message.to_string() }
    }

    /// - Batches keep listing order, cover it exactly
    /// - Batch closes at 100 calls or before passing 8 MiB; entry over the byte budget = alone
    #[test]
    fn raw_batches_split_in_order_on_either_budget() {
        let entry = |encoded_len: u32| MempoolListed {
            txid: TransactionId::from([0; 32]),
            fee: zaino_primitives::types::Zatoshis::ZERO,
            encoded_len,
        };
        let sizes = |listed: &[MempoolListed]| {
            raw_batches(listed).iter().map(|batch| batch.len()).collect::<Vec<_>>()
        };

        assert_eq!(sizes(&[]), Vec::<usize>::new());
        assert_eq!(sizes(&vec![entry(250); 250]), [100, 100, 50]);
        let mib = 1 << 20;
        assert_eq!(sizes(&[entry(3 * mib), entry(3 * mib), entry(3 * mib)]), [2, 1]);
        assert_eq!(sizes(&[entry(8 * mib), entry(1)]), [1, 1]);
        assert_eq!(sizes(&[entry(1), entry(9 * mib), entry(1)]), [1, 1, 1]);
    }

    /// - Both not-found codes = absent answer (missing block as a failure = sync stalls on a
    ///   healthy validator)
    /// - `getblockhash` above the tip likewise, zebrad's `-32602` too (validator behind ≠ failed
    ///   poll)
    /// - Every other code + every transport failure = failure (outage never reads as empty chain)
    #[test]
    fn only_not_found_codes_are_absent_answers() {
        let absent = || GetAtHeightError::HeightNotFound(Height::try_from(42u32).expect("h"));
        let classifiers: [(&str, &[i64]); 2] =
            [("not found", &NOT_FOUND_CODES), ("above the tip", &ABOVE_TIP_CODES)];
        for (case, codes) in classifiers {
            for &code in codes {
                let classified = absent_on(codes, rpc(code, case), absent);
                assert!(matches!(classified, QueryError::Domain(_)), "{case} {code}");
            }
            for error in [-1, -3, -20, -22, -25, -28, -32_600]
                .map(|code| rpc(code, "something else"))
                .into_iter()
                .chain([RpcError::Status(503)])
            {
                let classified = absent_on(codes, error, absent);
                assert!(matches!(classified, QueryError::NonDomain(_)), "{case}: {classified:?}");
            }
        }
        let classified = absent_or_fetch(rpc(-32602, "past the tip"), absent);
        assert!(
            matches!(classified, QueryError::NonDomain(_)),
            "-32602 absent only for getblockhash"
        );
    }

    /// - Rejections carry the reason (the only useful part)
    /// - Warming-up node = transaction never considered
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

    /// - `-32601` = no mempool on this node (stop asking)
    /// - zebrad's own `-1` below the network tip = inactive
    /// - every other code (bare `-1` included) = failure worth re-polling
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
