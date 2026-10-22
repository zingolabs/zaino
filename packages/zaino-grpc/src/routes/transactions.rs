//! Transaction methods: `SendTransaction` (submitted through the chain view), `GetTransaction`
//! (consensus bytes forwarded from a validator, usage.md "Why `GetTransaction` forwards"; no
//! validator answering = the served mempool, else the peers' block)

use http::Response;
use tonic::{body::Body, Status};
use zaino_chainview::{ChainView, SubmitError};
use zaino_grpc_snapshot::GrpcSnapshot;
use zaino_persistence::MapRead;
use zaino_primitives::types::TransactionLocation;
use zaino_proto::proto::service::{self, RawTransaction};
use zaino_source::{ChainDataSource, GetTransactionError, QueryError};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::TxId;

use super::mempool::unmined;
use crate::wire::{frame, unary_response};

/// `SendTransaction`, answered as lightwalletd does (a rejection = a reply, only unreachable = a
/// status: wallet tells "no" from "unreachable")
pub(crate) async fn send_transaction<S: ChainDataSource>(
    chain_view: &ChainView<S>,
    decoded_tx: service::RawTransaction,
) -> Result<Response<Body>, Status> {
    let broadcast = async {
        // refused here = never relayed
        let validated = chain_view.validate_transaction(decoded_tx.data.to_vec()).await?;
        chain_view.broadcast_transaction(validated).await
    };
    let response = match broadcast.await {
        // `sendrawtransaction`'s JSON result: quoted display-order txid (lightwalletd relays it)
        Ok(txid) => service::SendResponse { error_code: 0, error_message: format!("\"{txid}\"") },
        // gRPC OK + the validator's code + message verbatim (zingolib substring-matches it)
        Err(SubmitError::Rejected(rejection)) => service::SendResponse {
            error_code: rejection.code(),
            error_message: rejection.message().to_owned(),
        },
        Err(unreachable @ SubmitError::Unreachable { .. }) => {
            return Err(Status::unavailable(unreachable.to_string()))
        }
    };
    Ok(unary_response(frame(&response)))
}

/// `GetTransaction`: `hash` arm only (`TxFilter`'s `(block, index)` = positional, no index maps it)
pub(crate) async fn get_transaction<S: ChainDataSource, V: MapRead>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    tx_filter: service::TxFilter,
) -> Result<Response<Body>, Status> {
    let txid: [u8; 32] = tx_filter
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| Status::invalid_argument("txid must be 32 bytes, in protocol order"))?;

    let transaction =
        fetch_raw_transaction(validators, grpc_snapshot, TxId::from_bytes(txid)).await?;
    Ok(unary_response(frame(&transaction)))
}

/// Consensus bytes of `txid` (`GetTransaction`, `GetTaddressTransactions`)
///
/// - whichever validator holds it, else (none answering) [`fetch_without_validators`]
/// - `NOT_FOUND` only when every validator asked said absent (a failure = maybe held)
pub(super) async fn fetch_raw_transaction<S: ChainDataSource, V: MapRead>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    txid: TxId,
) -> Result<RawTransaction, Status> {
    let found = validators.lookup(move |source| async move { source.get_transaction(txid).await });
    match found.await {
        Ok(answered) => Ok(raw_transaction_message(answered.value)),
        Err(unanswered) => match &unanswered.last {
            Some(QueryError::Domain(GetTransactionError::NotFound(txid))) => {
                Err(Status::not_found(format!("transaction not found: {txid}")))
            }
            _ => {
                let unavailable = || Status::unavailable(unanswered.to_string());
                fetch_without_validators(validators, grpc_snapshot, txid)
                    .await
                    .ok_or_else(unavailable)
            }
        },
    }
}

/// Validator's answer → wire `RawTransaction`
///
/// - `height` = mined height, or `0` = the wire's "in the mempool" (= no-height location)
/// - orphaned → unmined (one wire "no height", an abandoned branch must not read as confirmed)
fn raw_transaction_message(found: zaino_source::TransactionResponse) -> RawTransaction {
    let height = match found.location {
        TransactionLocation::BestChain(at) => at.into(),
        TransactionLocation::NonBestChain | TransactionLocation::Mempool => 0,
    };
    RawTransaction { data: found.bytes.into(), height }
}

/// No validator answering: the served mempool's bytes (unmined), else the block `txid_location`
/// names from the peers, checked and the transaction cut out (`None` = neither: no `[p2p]`, the
/// index off, or a misanswer)
async fn fetch_without_validators<S: ChainDataSource, V: MapRead>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    txid: TxId,
) -> Option<RawTransaction> {
    let mempool = grpc_snapshot.mempool().ok();
    if let Some(entry) = mempool.and_then(|mempool| mempool.entries().find(|e| e.txid == txid)) {
        return Some(unmined(&entry));
    }
    let mined = grpc_snapshot.served().ok()?.views().txid_location()?.mined(&txid)?;
    let raw = validators.peer_block_bytes(mined.block.hash).await?.value;
    let bytes = zaino_source::mined_transaction(&raw, mined.block, mined.index, txid)?;
    Some(RawTransaction { data: bytes.into(), height: u32::from(mined.block.height).into() })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_header_chain::HeaderChainSnapshot;
    use zaino_source::testing::MockChain;

    use crate::testing::{grpc_snapshots_held, routes_over};

    /// `SendTransaction` through the router, one validator holding the verified tip:
    /// - accepted = code 0 + quoted display-order txid (`sendrawtransaction`'s JSON result)
    /// - validator refusal = gRPC OK + its code + message verbatim (zingolib's
    ///   `classify_rejection` texts intact, Android reconciles only on an OK reply)
    /// - unparseable = gRPC OK + `-22`, no validator asked
    /// - unreachable = `UNAVAILABLE` (never a reply a wallet reads as the network's answer)
    #[tokio::test(start_paused = true)]
    async fn send_transaction_answers_like_lightwalletd() {
        use http::HeaderValue;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_source::SendRawTransactionError::Rejected;

        use crate::service::Method;
        use crate::testing::{decode_frames, dispatch, drained, framed_request, rounds};

        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let node = chain.validator(tip);
        let (routes, balancing, merge) = routes_over(&node);
        let chain_view = Arc::clone(&routes.chain_view);
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(merge.run(cancel.child_token()));
        chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let reader = chain_view.subscriber();
        rounds("tip held", || !reader.current().held_by().is_empty()).await;
        let mut router = dispatch(routes);
        let request = |data: Vec<u8>| {
            let message = service::RawTransaction { data: data.into(), height: 0 }.encode_to_vec();
            framed_request(Method::SendTransaction, message.into())
        };
        let mut reply = async |data: Vec<u8>| {
            let (chunks, trailing) =
                drained(router.call(request(data)).await.expect("router answers")).await;
            assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
            let [reply] = <[service::SendResponse; 1]>::try_from(decode_frames(&chunks.concat()))
                .expect("one reply");
            (reply.error_code, reply.error_message)
        };

        let (txid, sent) = zaino_source::testing::raw_transaction(9, 0);
        assert_eq!(reply(sent).await, (0, format!("\"{txid}\"")), "accepted");

        let verdicts = [
            (-1, "transaction already exists in mempool"),
            (-27, "transaction already in block chain"),
            (-1, "transaction dropped because it is already queued for download"),
            (-25, "bad-txns-inputs-spent"),
        ];
        for (seed, (code, message)) in (10..).zip(verdicts) {
            node.relay(Err(Rejected { code, message: message.to_owned() }));
            let refused = reply(zaino_source::testing::raw_transaction(seed, 0).1).await;
            assert_eq!(refused, (code, message.to_owned()), "the validator's, verbatim");
        }

        let checks = node.calls("checkadmission");
        let (code, _) = reply(vec![9u8; 8]).await;
        let asked = node.calls("checkadmission");
        assert_eq!((code, asked), (-22, checks), "unparseable: no validator asked");

        node.lacks(&["checkadmission"]);
        node.reachable(&["sendrawtransaction"], false);
        let unsent = router
            .call(request(zaino_source::testing::raw_transaction(20, 0).1))
            .await
            .expect("router answers");
        let status = unsent.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("14")), "unreachable: UNAVAILABLE");
        cancel.cancel();
    }

    /// No validator answering, `[p2p]` off: a transaction the served mempool holds = its bytes at
    /// height 0, any other = `UNAVAILABLE` (the peers' block path:
    /// `zaino_source::mined_transaction`'s fixture test)
    #[tokio::test(start_paused = true)]
    async fn with_no_validator_answering_the_served_mempool_answers_and_the_rest_is_unavailable() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(3);
        let (txid, raw) = zaino_source::testing::raw_transaction(7, 0);
        let node = chain.validator(tip);
        node.down(true);
        let (routes, balancing, _merge) = routes_over(&node);
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(balancing.run(cancel.child_token()));
        let ours = [(txid, bytes::Bytes::from(raw.clone()))];
        let held = TrustedValidatorSet::at([0]);
        let held = grpc_snapshots_held(&chain, tip, Vec::new(), held, &ours).load();
        let ask = |txid| fetch_raw_transaction(&routes.validators, &held, txid);
        let served = ask(txid).await.map(|tx| (tx.data.to_vec(), tx.height));
        let other = ask(TxId::from_bytes([9; 32])).await.map(|_| ());
        let codes = (served.map_err(|s| s.code()), other.map_err(|s| s.code()));
        assert_eq!(codes, (Ok((raw, 0)), Err(tonic::Code::Unavailable)));
        cancel.cancel();
    }
}
