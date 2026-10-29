//! Transaction methods: `SendTransaction` (submitted through the chain view), `GetTransaction`
//! (consensus bytes forwarded from a validator, usage.md "Why `GetTransaction` forwards"; no
//! validator answering = the served mempool, else the peers' block)

use http::Response;
use tonic::{body::Body, Status};
use zaino_chainview::{ChainView, SubmitError};
use zaino_grpc_snapshot::GrpcSnapshot;
use zaino_persistence::PinnedView;
use zaino_primitives::types::TransactionLocation;
use zaino_proto::proto::service::{self, RawTransaction};
use zaino_source::{ChainDataSource, GetTransactionError, QueryError};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::TxId;

use super::mempool::unmined;
use crate::read_lanes::{Lane, ReadLanes};
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
pub(crate) async fn get_transaction<S: ChainDataSource, V: PinnedView>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    reads: &ReadLanes,
    tx_filter: service::TxFilter,
) -> Result<Response<Body>, Status> {
    let txid: [u8; 32] = tx_filter
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| Status::invalid_argument("txid must be 32 bytes, in protocol order"))?;

    let transaction =
        fetch_raw_transaction(validators, grpc_snapshot, reads, TxId::from_bytes(txid)).await?;
    Ok(unary_response(frame(&transaction)))
}

/// Consensus bytes of `txid` (`GetTransaction`, `GetTaddressTransactions`)
///
/// - whichever validator holds it, else (none answering) [`fetch_without_validators`]
/// - `NOT_FOUND` only when every validator asked said absent (a failure = maybe held)
pub(super) async fn fetch_raw_transaction<S: ChainDataSource, V: PinnedView>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    reads: &ReadLanes,
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
                fetch_without_validators(validators, grpc_snapshot, reads, txid)
                    .await?
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
async fn fetch_without_validators<S: ChainDataSource, V: PinnedView>(
    validators: &TrafficBalancer<S>,
    grpc_snapshot: &GrpcSnapshot<V>,
    reads: &ReadLanes,
    txid: TxId,
) -> Result<Option<RawTransaction>, Status> {
    if let Some(entry) = grpc_snapshot.mempool().ok().and_then(|mempool| mempool.get(&txid)) {
        return Ok(Some(unmined(&entry)));
    }
    let Some(index) = grpc_snapshot.served().ok().and_then(|served| served.views().txid_location())
    else {
        return Ok(None);
    };
    let Some(mined) = reads.read(Lane::Point, move || index.mined(&txid)).await? else {
        return Ok(None);
    };
    let Some(raw) = validators.peer_block_bytes(mined.block.hash).await else { return Ok(None) };
    let bytes = zaino_source::mined_transaction(&raw.value, mined.block, mined.index, txid);
    let height = u32::from(mined.block.height).into();
    Ok(bytes.map(|bytes| RawTransaction { data: bytes.into(), height }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_header_chain::HeaderChainSnapshot;
    use zaino_source::testing::MockChain;

    use crate::testing::{fixed_snapshots, routes_over, MAINNET};

    /// One validator holding the verified tip:
    /// - validator refusal = gRPC OK + its code + message verbatim (zingolib's `classify_rejection`
    ///   texts intact, Android reconciles only on an OK reply)
    /// - unreachable = `UNAVAILABLE` (never a reply a wallet reads as the network's answer)
    #[tokio::test(start_paused = true)]
    async fn send_transaction_answers_like_lightwalletd() {
        use prost::Message as _;
        use tonic::Code;
        use tower::Service as _;
        use zaino_source::testing::{raw_transaction, within};
        use zaino_source::SendRawTransactionError::Rejected;

        use crate::service::Method;
        use crate::testing::{answer, decode_frames, dispatch, framed_request, until};

        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let node = chain.validator(tip);
        let (routes, balancing, merge) = routes_over(&node, MAINNET, &[]);
        let chain_view = Arc::clone(&routes.chain_view);
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(merge.run(cancel.child_token()));
        chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let reader = chain_view.subscriber();
        within("tip 5 held", until(|| !reader.current().held_by().is_empty())).await;
        let mut router = dispatch(routes);
        // (gRPC code, each SendResponse's (errorCode, errorMessage))
        let mut send = async |data: Vec<u8>| {
            let message = service::RawTransaction { data: data.into(), height: 0 }.encode_to_vec();
            let response =
                router.call(framed_request(Method::SendTransaction, message.into())).await;
            let (chunks, status) = answer(response.expect("infallible")).await;
            let replies = decode_frames::<service::SendResponse>(&chunks.concat());
            let replies: Vec<_> =
                replies.into_iter().map(|r| (r.error_code, r.error_message)).collect();
            (status.code(), replies)
        };

        let (txid, sent) = raw_transaction(9, 0);
        let quoted = format!("\"{txid}\"");
        assert_eq!(
            send(sent).await,
            (Code::Ok, vec![(0, quoted)]),
            "accepted: quoted display txid"
        );

        let verdicts = [
            (-1, "transaction already exists in mempool"),
            (-27, "transaction already in block chain"),
            (-1, "transaction dropped because it is already queued for download"),
            (-25, "bad-txns-inputs-spent"),
        ];
        for (seed, (code, message)) in (10..).zip(verdicts) {
            node.relay(Err(Rejected { code, message: message.to_owned() }));
            let refused = send(raw_transaction(seed, 0).1).await;
            assert_eq!(
                refused,
                (Code::Ok, vec![(code, message.to_owned())]),
                "{message}: verbatim"
            );
        }

        let asked_before = node.calls("checkadmission");
        let (code, replies) = send(vec![9u8; 8]).await;
        let codes: Vec<_> = replies.iter().map(|(code, _)| *code).collect();
        assert_eq!((code, codes), (Code::Ok, vec![-22]), "unparseable: {replies:?}");
        assert_eq!(node.calls("checkadmission"), asked_before, "unparseable: no validator asked");

        node.lacks(&["checkadmission"]);
        node.reachable(&["sendrawtransaction"], false);
        let unsent = send(raw_transaction(20, 0).1).await;
        assert_eq!(unsent, (Code::Unavailable, Vec::new()), "validator unreachable");
        cancel.cancel();
    }

    /// No validator answering, `[p2p]` off (the peers' block path:
    /// `zaino_source::mined_transaction`'s fixture test)
    #[tokio::test(start_paused = true)]
    async fn with_no_validator_answering_the_served_mempool_answers_and_the_rest_is_unavailable() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(3);
        let (txid, raw) = zaino_source::testing::raw_transaction(7, 0);
        let node = chain.validator(tip);
        node.down(true);
        let (routes, balancing, _never_merged) = routes_over(&node, MAINNET, &[]);
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(balancing.run(cancel.child_token()));
        let ours = [(txid, bytes::Bytes::from(raw.clone()))];
        let held = fixed_snapshots(&chain, tip, Vec::new(), TrustedValidatorSet::at([0]), &ours);
        let held = held.load();
        let reads = ReadLanes::new(&crate::GrpcLimits::default());

        let served = fetch_raw_transaction(&routes.validators, &held, &reads, txid).await;
        let unheld = TxId::from_bytes([9; 32]);
        let other = fetch_raw_transaction(&routes.validators, &held, &reads, unheld).await;

        let served = served.map(|tx| (tx.data.to_vec(), tx.height));
        assert_eq!(
            served.as_ref().ok(),
            Some(&(raw, 0)),
            "served mempool's bytes, unmined: {served:?}"
        );
        let code = other.as_ref().map(|_| ()).map_err(Status::code);
        assert_eq!(code, Err(tonic::Code::Unavailable), "held nowhere we can ask: {other:?}");
        cancel.cancel();
    }
}
