//! [`ZcashLegacyRpcClient`]: every question over the validator's JSON-RPC (zebrad or zcashd)
//!
//! - blocks = `getblock … 0` hex in JSON: one SIMD hex pass into an exact-size buffer

use serde_json::Value;
use zaino_primitives::types::Block;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, TxId};

use crate::endpoint::validator_url;
use crate::rpc::{Call, RpcClient, RpcClientConfig, RpcError, REPLY_ENVELOPE_BYTES};
use crate::{
    decode, json, BlockLink, BlockLinks, BlockchainInfo, Endpoint, EndpointError, FailureMode,
    GetAtHeightError, GetBlockByHashError, GetMempoolListingError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolListed, MetadataReading, NonDomainError, PollReading, QueryError,
    RawMempoolTransactions, ResponseSizeCap, SendRawTransactionError, Telemetry,
    TransactionResponse,
};

/// Validator's link
///
/// - Single attempt per call (retries, concurrency = the caller's balancer)
/// - `rpc` `pub(crate)`: [`ZebraIndexerGrpcClient`](crate::ZebraIndexerGrpcClient) asks the rest
///   through it + shares its metric label
pub struct ZcashLegacyRpcClient {
    pub(crate) rpc: RpcClient,
}

impl ZcashLegacyRpcClient {
    /// Validator at `endpoint`, unprobed (down at boot = the first call's failure + the caller's
    /// retry, never a boot failure)
    pub fn new(endpoint: &Endpoint) -> Result<Self, EndpointError> {
        let rpc = RpcClient::new(RpcClientConfig {
            url: validator_url(&endpoint.address)?,
            name: endpoint.address.clone(),
            auth: endpoint.auth.credentials()?,
            timeouts: endpoint.timeouts,
            limits: endpoint.limits,
        })
        .map_err(EndpointError::Client)?;
        Ok(Self { rpc })
    }

    /// Call + parse (`answer` = the method's own verdicts among its errors, [`classify`])
    async fn call_parsed<T, P, E>(
        &self,
        method: &str,
        params: Vec<Value>,
        parse: impl FnOnce(Value) -> Result<T, P>,
        answer: impl FnOnce(&RpcError) -> Option<E>,
    ) -> Result<T, QueryError<E>>
    where
        P: Into<Box<dyn std::error::Error + Send + Sync>>,
        E: std::fmt::Debug + std::fmt::Display,
    {
        let call = self.rpc.call::<Value>(method, params).await;
        let value = call.map_err(|error| classify(error, answer))?;
        parse(value).map_err(|e| QueryError::NonDomain(from_parse(e)))
    }
}

fn from_parse(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> NonDomainError {
    NonDomainError::from_cause(FailureMode::Parse, e)
}

/// JSON-RPC "method not found"
const METHOD_NOT_FOUND: i64 = -32601;

/// One call's error as a port reads it: `-32601` = `Unsupported` (whatever the method), else
/// `answer`'s verdict, else no answer
fn classify<E>(error: RpcError, answer: impl FnOnce(&RpcError) -> Option<E>) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    if let RpcError::Rpc { code: METHOD_NOT_FOUND, .. } = error {
        return QueryError::Unsupported;
    }
    match answer(&error) {
        Some(answer) => QueryError::Domain(answer),
        None => QueryError::NonDomain(error.into()),
    }
}

/// Batch item's outcome: `Ok(Err(answer))` = the validator's verdict, `Err` = no answer
///
/// - `-32601` = no answer unless `answer` reads it (a node without a method it batches = broken)
fn batch_item<T, E>(
    reply: Result<T, RpcError>,
    answer: impl FnOnce(&RpcError) -> Option<E>,
) -> Result<Result<T, E>, NonDomainError> {
    match reply {
        Ok(value) => Ok(Ok(value)),
        Err(error) => answer(&error).map(Err).ok_or_else(|| error.into()),
    }
}

/// zebrad's "no such object": `-8` on `getblock <height>`, `-5` on hash/txid lookups
///
/// - both also "malformed parameter" upstream (safe: every param rendered from a domain type)
fn not_found(error: &RpcError) -> bool {
    matches!(error, RpcError::Rpc { code: -5 | -8, .. })
}

/// `getblockhash` above the tip: zebrad `-32602` (index past the tip), zcashd `-8` (out of range)
fn above_tip(error: &RpcError) -> bool {
    matches!(error, RpcError::Rpc { code: -8 | -32602, .. })
}

/// zebrad's `-1` verdicts on the transaction itself (zebrad/src/components/mempool/error.rs)
///
/// - the rest of its `-1`s ("queue is full", "mempool is disabled") = node state: next validator
const MEMPOOL_VERDICTS: [&str; 4] = [
    "transaction already exists in mempool",
    "already queued for download",
    "will be rejected from the mempool",
    "transaction is non-standard",
];

/// `-22` unparseable, `-25` to `-27` (both inclusive) declined, a `-1` in [`MEMPOOL_VERDICTS`]:
/// answers about the transaction, code + reason kept verbatim
fn submission_rejection(error: &RpcError) -> Option<SendRawTransactionError> {
    let RpcError::Rpc { code, message } = error else { return None };
    let rejected = || {
        let code = i32::try_from(*code).ok()?;
        Some(SendRawTransactionError::Rejected { code, message: message.clone() })
    };
    match code {
        -22 => Some(SendRawTransactionError::Malformed(message.clone())),
        -27..=-25 => rejected(),
        -1 if MEMPOOL_VERDICTS.iter().any(|verdict| message.contains(verdict)) => rejected(),
        _ => None,
    }
}

/// zebrad's `-1` on `getrawmempool true` below the network tip
///
/// - `-1` = zebra's catch-all `Misc` (message = only discriminant)
/// - zebrad/src/components/mempool.rs (`Request::FullTransactions` while disabled)
const MEMPOOL_INACTIVE: &str = "mempool is not active";

/// `getrawmempool` verdicts (answers, not failures)
///
/// - `-32601` = `Unavailable` (no mempool on this node)
/// - `-1` + [`MEMPOOL_INACTIVE`] = `Inactive` (no mempool until caught up)
fn listing_verdict(error: &RpcError) -> Option<GetMempoolListingError> {
    match error {
        RpcError::Rpc { code: METHOD_NOT_FOUND, .. } => Some(GetMempoolListingError::Unavailable),
        RpcError::Rpc { code: -1, message } if message.starts_with(MEMPOOL_INACTIVE) => {
            Some(GetMempoolListingError::Inactive)
        }
        _ => None,
    }
}

/// `getblockchaininfo` (no tip read = the whole poll fails)
fn read_info(reply: Result<Value, RpcError>) -> Result<BlockchainInfo, NonDomainError> {
    json::parse_blockchain_info(reply?).map_err(from_parse)
}

/// `getrawmempool true` (garbled listing = the whole poll fails)
fn read_listing(
    reply: Result<Value, RpcError>,
) -> Result<Result<Vec<MempoolListed>, GetMempoolListingError>, NonDomainError> {
    match batch_item(reply, listing_verdict)? {
        Ok(listing) => Ok(Ok(json::parse_mempool_listing(listing).map_err(from_parse)?)),
        Err(answer) => Ok(Err(answer)),
    }
}

/// `getblockhash <height>`: its own outcome (the rest of the poll stands)
fn read_held(
    height: BlockHeight,
    reply: Result<Value, RpcError>,
) -> Result<BlockHash, QueryError<GetAtHeightError>> {
    let absent =
        |error: &RpcError| above_tip(error).then_some(GetAtHeightError::HeightNotFound(height));
    let hash = reply.map_err(|error| classify(error, absent))?;
    json::block_hash(hash).map_err(|e| QueryError::NonDomain(from_parse(e)))
}

/// `getpeerinfo` + `getinfo` + `getdeprecationinfo`: each failed read named, never the poll's
fn read_metadata(
    peers: Result<Value, RpcError>,
    info: Result<Value, RpcError>,
    deprecation: Result<Value, RpcError>,
) -> MetadataReading {
    let deprecation = match deprecation {
        Ok(value) => Ok(Some(value)),
        // zebrad < 6.3
        Err(RpcError::Rpc { code: METHOD_NOT_FOUND, .. }) => Ok(None),
        Err(error) => Err(NonDomainError::from(error)),
    };
    let peers = peers.map_err(NonDomainError::from);
    let peers = peers.and_then(|peers| json::parse_peer_info(peers).map_err(from_parse));
    let release = info
        .map_err(NonDomainError::from)
        .and_then(|info| json::parse_node_release(info, deprecation?).map_err(from_parse));
    let mut failed = Vec::new();
    let peers = peers.map_err(|cause| failed.push(("peer list", cause))).ok();
    let release = release.map_err(|cause| failed.push(("release", cause))).ok();
    MetadataReading { answered: Telemetry { peers, release }, failed }
}

/// Headers per `getblockheader` batch (~3 KB hex each: ~1.5 MB reply)
const LINK_BATCH_CALLS: usize = 500;

impl crate::ChainDataSource for ZcashLegacyRpcClient {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        let params = vec![hash.to_string().into(), 0.into()];
        let absent =
            |error: &RpcError| not_found(error).then_some(GetBlockByHashError::NotFound(hash));
        let json::HexBytes(raw) =
            self.rpc.call("getblock", params).await.map_err(|error| classify(error, absent))?;
        decode::served_block(&raw)
    }

    async fn get_blocks_by_height(
        &self,
        heights: &[BlockHeight],
    ) -> Result<Vec<Block>, QueryError<GetAtHeightError>> {
        let call = |height: &BlockHeight| Call {
            method: "getblock",
            params: vec![u32::from(*height).to_string().into(), 0.into()],
        };
        let mut blocks = Vec::with_capacity(heights.len());
        for batch in heights.chunks(self.rpc.response_size_cap().blocks_per_batch().get()) {
            let calls = batch.iter().map(call).collect();
            let replies = self.rpc.call_batch::<json::HexBytes>("getblock", calls);
            let replies = replies.await.map_err(|error| QueryError::NonDomain(error.into()))?;
            for (height, reply) in batch.iter().zip(replies) {
                let absent = |error: &RpcError| {
                    not_found(error).then_some(GetAtHeightError::HeightNotFound(*height))
                };
                let json::HexBytes(raw) = reply.map_err(|error| classify(error, absent))?;
                blocks.push(decode::served_block(&raw)?);
            }
        }
        Ok(blocks)
    }

    /// Raw form (verbose = two extra state reads per header on zebrad)
    async fn get_block_links(&self, heights: &[BlockHeight]) -> Result<BlockLinks, NonDomainError> {
        let mut links = Vec::with_capacity(heights.len());
        for batch in heights.chunks(LINK_BATCH_CALLS) {
            let calls = batch
                .iter()
                .map(|height| Call {
                    method: "getblockheader",
                    params: vec![u32::from(*height).to_string().into(), false.into()],
                })
                .collect();
            let replies = self.rpc.call_batch::<json::HexBytes>("getblockheader", calls).await?;
            for (height, reply) in batch.iter().zip(replies) {
                let absent = |error: &RpcError| {
                    not_found(error).then_some(GetAtHeightError::HeightNotFound(*height))
                };
                let link = reply.map(|json::HexBytes(header)| BlockLink { header });
                links.push(batch_item(link, absent)?);
            }
        }
        Ok(links)
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[BlockHeight],
    ) -> Result<PollReading, NonDomainError> {
        let call = |method, params| Call { method, params };
        let mut calls =
            vec![call("getblockchaininfo", vec![]), call("getrawmempool", vec![true.into()])];
        calls.extend(holds.iter().map(|h| call("getblockhash", vec![u32::from(*h).into()])));
        if metadata {
            calls.extend(["getpeerinfo", "getinfo", "getdeprecationinfo"].map(|m| call(m, vec![])));
        }
        let mut replies = self.rpc.call_batch::<Value>("poll", calls).await?.into_iter();
        let mut next = || replies.next().expect("a batch answers every call (parse_batch)");

        let info = read_info(next())?;
        let listing = read_listing(next())?;
        let held = holds.iter().map(|height| read_held(*height, next())).collect();
        let metadata = metadata.then(|| read_metadata(next(), next(), next()));
        Ok(PollReading { info, listing, held, metadata })
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        let mut fetched = Vec::with_capacity(listed.len());
        let budget = raw_batch_bytes(self.rpc.response_size_cap());
        for batch in raw_batches(listed, budget) {
            let calls = batch
                .iter()
                .map(|entry| Call {
                    method: "getrawtransaction",
                    params: vec![entry.txid.to_string().into(), 0.into()],
                })
                .collect();
            let replies = self.rpc.call_batch::<json::HexBytes>("getrawtransaction", calls).await?;
            for (entry, reply) in batch.iter().zip(replies) {
                let absent = |error: &RpcError| {
                    not_found(error).then_some(GetRawMempoolTransactionError::NotFound(entry.txid))
                };
                fetched.push(batch_item(reply.map(|json::HexBytes(raw)| raw), absent)?);
            }
        }
        Ok(fetched)
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TxId, QueryError<SendRawTransactionError>> {
        let params = vec![const_hex::encode(transaction).into()];
        self.call_parsed("sendrawtransaction", params, json::txid, submission_rejection).await
    }

    async fn check_admission(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TxId, QueryError<SendRawTransactionError>> {
        let params = vec![const_hex::encode(transaction).into()];
        self.call_parsed("checkadmission", params, json::txid, submission_rejection).await
    }

    /// Verbosity 1: the height places it (mined vs mempool)
    async fn get_transaction(
        &self,
        txid: TxId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        let params = vec![txid.to_string().into(), Value::Number(1.into())];
        let absent =
            |error: &RpcError| not_found(error).then_some(GetTransactionError::NotFound(txid));
        self.call_parsed("getrawtransaction", params, json::parse_transaction, absent).await
    }
}

/// Calls per batch (one batch holds one connection for its whole reply)
const RAW_BATCH_CALLS: usize = 100;

/// Raw bytes per `getrawtransaction` batch: its hex reply (2x) + envelopes within `cap`
fn raw_batch_bytes(cap: ResponseSizeCap) -> u64 {
    ((cap.bytes() - RAW_BATCH_CALLS * REPLY_ENVELOPE_BYTES) / 2) as u64
}

/// `listed` cut in order into batches within both budgets (an entry over `budget` alone)
fn raw_batches(listed: &[MempoolListed], budget: u64) -> Vec<&[MempoolListed]> {
    let mut batches = Vec::new();
    let (mut start, mut bytes) = (0, 0u64);
    for (index, entry) in listed.iter().enumerate() {
        let len = u64::from(entry.encoded_len);
        let full = index - start == RAW_BATCH_CALLS || bytes + len > budget;
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
    /// - Batch closes at 100 calls or before passing the budget (8 MiB), entry over it = alone
    /// - Budget at the 32 MiB cap = 100 hex replies + envelopes
    #[test]
    fn raw_batches_split_in_order_on_either_budget() {
        let entry = |encoded_len: u32| MempoolListed {
            txid: TxId::from_bytes([0; 32]),
            fee: zcash_protocol::value::Zatoshis::ZERO,
            encoded_len,
        };
        let sizes = |listed: &[MempoolListed]| {
            raw_batches(listed, 8 << 20).iter().map(|batch| batch.len()).collect::<Vec<_>>()
        };
        assert_eq!(raw_batch_bytes(ResponseSizeCap::MAX), 16_726_016);

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
    /// - `-32601` = `Unsupported` under every method's verdicts (an older node, never a failure)
    /// - Every other code + every transport failure = failure (outage never reads as empty chain)
    #[test]
    fn only_not_found_codes_are_absent_answers_and_a_missing_method_is_unsupported() {
        let absent = GetAtHeightError::HeightNotFound(BlockHeight::from(42));
        for (case, verdict, codes) in [
            ("not found", not_found as fn(&RpcError) -> bool, [-5, -8]),
            ("above the tip", above_tip, [-8, -32602]),
        ] {
            let classified =
                |error| classify(error, |error| verdict(error).then(|| absent.clone()));
            for code in codes {
                let classified = classified(rpc(code, case));
                assert!(matches!(classified, QueryError::Domain(_)), "{case} {code}");
            }
            for error in [-1, -3, -20, -22, -25, -28, -32_600]
                .map(|code| rpc(code, "something else"))
                .into_iter()
                .chain([RpcError::Status(503)])
            {
                let classified = classified(error);
                assert!(matches!(classified, QueryError::NonDomain(_)), "{case}: {classified:?}");
            }
            let missing = classified(rpc(METHOD_NOT_FOUND, "Method not found"));
            assert!(matches!(missing, QueryError::Unsupported), "{case}");
        }
        assert!(!not_found(&rpc(-32602, "past the tip")), "-32602 absent only for getblockhash");
    }

    /// - Rejections carry the validator's code + reason verbatim (wallets match the text)
    /// - zebrad's `-1` = verdict only when it names the transaction
    /// - Node state / warming up = never considered (next validator)
    #[test]
    fn submission_rejections_carry_the_validators_code_and_reason() {
        use SendRawTransactionError::{Malformed, Rejected};
        let rejected = |code: i32, message: &str| Some(Rejected { code, message: message.into() });
        let in_mempool = "transaction already exists in mempool";
        let queued = "transaction dropped because it is already queued for download";
        let cached = "any transaction with the same effects will be rejected from the mempool \
                      until the next chain tip block: spent nullifier";
        let cases = [
            (-22, "tx unparseable", Some(Malformed("tx unparseable".into()))),
            (-25, "bad-txns-inputs-spent", rejected(-25, "bad-txns-inputs-spent")),
            (-26, "tx-overwinter-expired", rejected(-26, "tx-overwinter-expired")),
            (
                -27,
                "transaction already in block chain",
                rejected(-27, "transaction already in block chain"),
            ),
            (-1, in_mempool, rejected(-1, in_mempool)),
            (-1, queued, rejected(-1, queued)),
            (-1, cached, rejected(-1, cached)),
            (-1, "transaction dropped because the queue is full", None),
            (-1, "mempool is disabled since synchronization is behind the chain tip", None),
            (-28, "warming up", None),
        ];
        for (code, message, expected) in cases {
            assert_eq!(submission_rejection(&rpc(code, message)), expected, "{code}");
        }
    }

    /// `checkadmission` (zebra fork): `sendrawtransaction`'s verdicts
    ///
    /// - `-25` / a `-1` mempool verdict = refused, verbatim
    /// - Any other `-1` (inactive mempool, verifier timeout) = no verdict, a failure
    #[test]
    fn admission_checks_are_a_refusal_or_no_verdict() {
        let bad = "failed to validate tx: bad-txns-inputs-missingorspent";
        let refused = classify(rpc(-25, bad), submission_rejection);
        let verbatim = SendRawTransactionError::Rejected { code: -25, message: bad.into() };
        assert!(matches!(&refused, QueryError::Domain(r) if *r == verbatim), "{refused:?}");
        let non_standard = classify(rpc(-1, "transaction is non-standard"), submission_rejection);
        assert!(matches!(non_standard, QueryError::Domain(_)));
        for message in ["mempool is not active: wait for Zebra to sync to the tip", "timeout"] {
            let no_verdict = classify(rpc(-1, message), submission_rejection);
            assert!(matches!(no_verdict, QueryError::NonDomain(_)), "{message}");
        }
    }

    /// - zebrad's own `-1` below the network tip = inactive
    /// - no `getrawmempool` (`-32601`) = unavailable
    /// - every other code (bare `-1` included) = failure worth re-polling
    #[test]
    fn an_inactive_mempool_is_zebrads_own_minus_one() {
        use GetMempoolListingError::{Inactive, Unavailable};
        let listing = |error| batch_item::<(), _>(Err(error), listing_verdict);
        let inactive = listing(rpc(-1, "mempool is not active: wait for Zebra to sync to the tip"));
        assert!(matches!(inactive, Ok(Err(Inactive))));
        let missing = listing(rpc(METHOD_NOT_FOUND, "Method not found"));
        assert!(matches!(missing, Ok(Err(Unavailable))));
        for code in [-5, -8, -28, -1, -32603] {
            assert!(listing(rpc(code, "something else")).is_err(), "code {code}");
        }
    }
}
