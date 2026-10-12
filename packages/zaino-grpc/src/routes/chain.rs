//! Methods no index backs: `SendTransaction` (chain view), mempool methods + `GetLightdInfo`
//! (the snapshot's chain view half), `GetTransaction` (forwarded: consensus bytes, usage.md "Why
//! `GetTransaction` forwards")
//!
//! - `block_height` = what Zaino serves, not the validator's tip (a wallet gates its sync on it)
//! - `estimated_height` = the validator's network-tip estimate ("how far behind")

use http::{HeaderValue, Response};
use http_body::Frame;
use http_body_util::StreamBody;
use tonic::{body::Body, Status};
use zaino_chainview::{ChainView, MempoolEntry, SubmitError};
use zaino_index_compact_block::project_tx_at;
use zaino_primitives::network::chain_name;
use zaino_primitives::types::{TransactionId, TransactionLocation, Zatoshis};
use zaino_proto::proto::service::{self as proto, LightdInfo, RawTransaction};
use zaino_snapshot::{Lightd, Snapshot};
use zaino_source::{ChainDataSource, GetTransactionError, QueryError};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::consensus::NetworkUpgrade;

use crate::wire::{self, decode_request, frame, status_response, trailers};

/// `GetMempoolTx`: servable mempool minus what the client holds, compacted
///
/// - Materialised, not lazy (CPU over in-memory bytes)
/// - consensus parse once per entry ([`Projection`](zaino_chainview::Projection), slot 0, every
///   pool); per request only the slot and the pool selection, over the cached bytes
/// - a transaction the selection leaves with no component is dropped, as from a block
///   ([`project_tx_at`]); slots stay the listing's
pub(crate) async fn mempool_tx<V, B>(
    snap: &Snapshot<V>,
    body: B,
) -> Result<Vec<bytes::Bytes>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::GetMempoolTxRequest = decode_request(body).await?;
    let pools = wire::pools(&request.pool_types)?;

    let mempool = snap.mempool().map_err(wire::unavailable)?;

    let mut framed = Vec::new();
    for (slot, entry) in mempool.excluding(&request.exclude_txid_suffixes).into_iter().enumerate() {
        let rendered = entry.projection.get_or_render(|| project(&entry.raw, entry.fee))?;
        framed.extend(project_tx_at(&rendered, slot as u64, pools));
    }
    Ok(framed)
}

/// One mempool transaction as a `CompactTx` (slot 0, every pool), encoded: the cached form
fn project(raw: &[u8], fee: Option<Zatoshis>) -> Result<bytes::Bytes, Status> {
    let parsed = zaino_source::decode_transaction(raw)
        .map_err(|failed| Status::internal(format!("mempool transaction: {failed}")))?;
    let tx = zaino_index_compact_block::compact_tx(0, &parsed, fee);
    Ok(bytes::Bytes::from(prost::Message::encode_to_vec(&tx)))
}

/// Submitted; rejection = domain answer, only unreachable = a status (wallet tells "no"
/// from "unreachable")
pub(crate) async fn send<S: ChainDataSource, B>(
    view: &ChainView<S>,
    body: B,
) -> Result<bytes::Bytes, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let raw: proto::RawTransaction =
        wire::decode_request_within(body, wire::request_limit::TRANSACTION).await?;
    send_reply(view.submit(raw.data.to_vec()).await).map(|reply| frame(&reply))
}

/// Submission outcome as lightwalletd answers it
///
/// - accepted: `sendrawtransaction`'s raw JSON result = quoted display-order txid (lightwalletd
///   relays it untouched)
/// - rejected: gRPC OK, the validator's code + message verbatim (zingolib substring-matches it)
fn send_reply(outcome: Result<TransactionId, SubmitError>) -> Result<proto::SendResponse, Status> {
    match outcome {
        Ok(txid) => Ok(proto::SendResponse { error_code: 0, error_message: format!("\"{txid}\"") }),
        Err(SubmitError::Rejected(rejection)) => Ok(proto::SendResponse {
            error_code: rejection.code(),
            error_message: rejection.message().to_owned(),
        }),
        Err(unreachable @ SubmitError::Unreachable { .. }) => {
            Err(Status::unavailable(unreachable.to_string()))
        }
    }
}

/// Unmined by construction; the wire spells that `height: 0`
fn unmined(entry: &MempoolEntry) -> proto::RawTransaction {
    proto::RawTransaction { data: entry.raw.clone(), height: 0 }
}

/// `GetMempoolStream`: the servable mempool as one chunk, then each arrival, closed once the
/// served tip moves (no holder of the verified best = `UNAVAILABLE`, never a silent stream)
///
/// - end ⇒ the next load serves the new tip (`GetLatestBlock` ≥ it: G5)
/// - each record encoded once (first subscriber to reach it); the rest share it by refcount
pub(crate) fn mempool_stream<V>(snap: &Snapshot<V>) -> Response<Body> {
    let tail = match snap.mempool_stream() {
        Ok(tail) => tail,
        Err(why) => return status_response(wire::unavailable(why)),
    };
    let opening = tail.opening_rendered(|entries| {
        wire::frame_all(&entries.iter().map(unmined).collect::<Vec<_>>())
    });
    let opening = (!opening.is_empty()).then(|| Ok::<_, Status>(Frame::data(opening)));

    // Tail in the unfold state, not captured (borrowed mutably across an await: `FnMut` can't)
    let arrivals = futures::stream::unfold(Some(tail), move |state| async move {
        let mut tail = state?;

        let Some(logged) = tail.next().await else {
            return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
        };

        let record = logged.rendered(|entry| frame(&unmined(entry)));
        Some((Ok::<_, Status>(Frame::data(record)), Some(tail)))
    });
    let frames = futures::StreamExt::chain(futures::stream::iter(opening), arrivals);

    let mut response = Response::new(Body::new(StreamBody::new(frames)));
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

    response
}

/// `GetTransaction`: `hash` arm only (`TxFilter`'s `(block, index)` = positional, no index maps it)
pub(crate) async fn transaction<S: ChainDataSource, B>(
    validators: &TrafficBalancer<S>,
    body: B,
) -> Result<bytes::Bytes, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let filter: proto::TxFilter = decode_request(body).await?;
    let txid: [u8; 32] = filter
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| Status::invalid_argument("txid must be 32 bytes, in protocol order"))?;
    let found = raw_transaction(validators, txid.into()).await?;
    Ok(frame(&found))
}

/// Consensus bytes from whichever validator holds it (`GetTransaction`, `GetTaddressTransactions`
/// bytes)
///
/// - `height` = mined height, or `0` = the wire's "in the mempool" (= no-height location)
/// - `NOT_FOUND` only when every validator asked said absent (a failure = maybe held)
pub(super) async fn raw_transaction<S: ChainDataSource>(
    validators: &TrafficBalancer<S>,
    txid: TransactionId,
) -> Result<RawTransaction, Status> {
    let found =
        validators.transaction(txid).await.map_err(|unanswered| match &unanswered.last {
            Some(QueryError::Domain(GetTransactionError::NotFound(txid))) => {
                Status::not_found(format!("transaction not found: {txid}"))
            }
            _ => Status::unavailable(unanswered.to_string()),
        })?;
    let found = found.value;

    // Orphaned → unmined (one wire "no height"; an abandoned branch must not read as confirmed)
    let height = match found.location {
        TransactionLocation::BestChain(at) => at.into(),
        TransactionLocation::NonBestChain | TransactionLocation::Mempool => 0,
    };

    Ok(RawTransaction { data: found.bytes.into(), height })
}

/// Serving metadata from one snapshot: no validator read (served degraded too)
///
/// - `blockHeight` = the served tip `GetLatestBlock` serves; `estimatedHeight` = the verified
///   best (the network's tip as far as the header chain knows)
/// - branch at the served tip + Sapling + the next upgrade = the schedule (mainnet/testnet
///   compiled, regtest its validator's at boot)
/// - `network` = declared, never read off the validator (zebra on regtest reports `"test"`)
/// - nothing served = `UNAVAILABLE`
/// - `lightwalletProtocolVersion` = vendored protos' release (pepper-sync refuses < v0.5.0)
pub(crate) fn lightd_info<V>(
    snap: &Snapshot<V>,
    network: NetworkType,
) -> Result<LightdInfo, Status> {
    snap.lightd().map(|lightd| render_lightd(lightd, network)).map_err(wire::unavailable)
}

fn render_lightd(lightd: Lightd, network: NetworkType) -> LightdInfo {
    let schedule = lightd.schedule;
    let next = schedule.next_after(lightd.served);
    let sapling = schedule.activation(NetworkUpgrade::Sapling);
    LightdInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        vendor: "zaino".to_string(),
        taddr_support: true,
        chain_name: chain_name(network).to_string(),
        sapling_activation_height: sapling.map_or(0, u64::from),
        consensus_branch_id: schedule.branch_at(lightd.served).to_string(),
        block_height: u64::from(lightd.served),
        estimated_height: u64::from(lightd.best),
        upgrade_name: next.map_or_else(String::new, |(upgrade, _)| upgrade.to_string()),
        upgrade_height: next.map_or(0, |(_, at)| u64::from(at)),
        lightwallet_protocol_version: zaino_proto::LIGHTWALLET_PROTOCOL_VERSION.to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use zaino_index_compact_block::Pools;
    use zaino_primitives::types::{Height, UpgradeSchedule};
    use zaino_proto::frame::split_frame;
    use zaino_proto::proto::service::PoolType;

    use http::HeaderValue;
    use zaino_header_chain::testing::HeaderViews;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::BlockRef;
    use zaino_source::testing::{raw_transaction, MockValidator, Port};

    use crate::testing::{dispatch, framed_request, indexed_at, published, routes_over};
    use crate::wire::path;

    /// Up to 10 poll rounds (paused clock: instant) until `done`
    async fn rounds(what: &str, done: impl Fn() -> bool) {
        for _ in 0..10 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        panic!("never {what}");
    }

    /// - `poolTypes` → pools served (pruning itself = compact-block's projection); unknown refused
    /// - Empty != all: wire pins it to the legacy shielded set (no transparent)
    #[test]
    fn pool_types_name_the_pools_empty_means_shielded_only_and_unknown_is_refused() {
        let pools = |named: &[PoolType]| {
            let raw: Vec<i32> = named.iter().map(|pool| *pool as i32).collect();
            crate::wire::pools(&raw).expect("known pools")
        };

        for (raw, value) in [(vec![PoolType::Invalid as i32], 0), (vec![2, 99, 3], 99)] {
            let refused = crate::wire::pools(&raw).expect_err("names no pool");
            let expected = format!(
                "poolTypes value {value} is not a pool \
                 (TRANSPARENT=1, SAPLING=2, ORCHARD=3, IRONWOOD=4)"
            );
            let got = (refused.code(), refused.message());
            assert_eq!(got, (tonic::Code::InvalidArgument, expected.as_str()), "{raw:?}");
        }

        let only = |sapling, orchard, ironwood, transparent| Pools {
            sapling,
            orchard,
            ironwood,
            transparent,
        };
        let (sapling_ironwood, every) = (
            [PoolType::Sapling, PoolType::Ironwood],
            [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard, PoolType::Ironwood],
        );
        assert_eq!(pools(&[]), only(true, true, true, false), "none named = shielded only");
        assert_eq!(pools(&[PoolType::Transparent]), only(false, false, false, true));
        assert_eq!(pools(&sapling_ironwood), only(true, false, true, false), "exactly its members");
        assert_eq!(pools(&every), Pools::ALL);
    }

    /// - Accepted = code 0 + quoted display-order txid (`sendrawtransaction`'s JSON result)
    /// - Rejected = gRPC OK + the validator's code + message verbatim (zingolib's
    ///   `classify_rejection` texts intact; Android reconciles only on an OK reply)
    /// - Unreachable = `UNAVAILABLE` (never a reply a wallet reads as the network's answer)
    #[test]
    fn a_submission_answers_like_lightwalletd() {
        use zaino_source::SendRawTransactionError::{Malformed, Rejected};

        let mut internal = [0u8; 32];
        internal[0] = 0x01;
        internal[31] = 0xff;
        let accepted = send_reply(Ok(TransactionId::from(internal))).expect("a reply");
        let quoted_display = format!("\"ff{}01\"", "00".repeat(30));
        assert_eq!((accepted.error_code, accepted.error_message), (0, quoted_display));

        let verdicts = [
            (-1, "transaction already exists in mempool"),
            (-27, "transaction already in block chain"),
            (-1, "transaction dropped because it is already queued for download"),
            (-25, "bad-txns-inputs-spent"),
        ];
        for (code, message) in verdicts {
            let rejection = Rejected { code, message: message.to_owned() };
            let reply = send_reply(Err(SubmitError::Rejected(rejection))).expect("a reply");
            assert_eq!((reply.error_code, reply.error_message.as_str()), (code, message));
        }
        let malformed = SubmitError::Rejected(Malformed("tx unparseable".into()));
        let reply = send_reply(Err(malformed)).expect("a reply");
        assert_eq!((reply.error_code, reply.error_message.as_str()), (-22, "tx unparseable"));

        let down = zaino_source::NonDomainError::new(zaino_source::FailureMode::Connection, "gone");
        let unreachable = send_reply(Err(SubmitError::Unreachable { attempted: 2, cause: down }));
        let status = unreachable.expect_err("a status, never a reply");
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }

    /// Mainnet's compiled schedule: branch at the served height (NU6.2 from 3,364,600 below
    /// NU6.3, NU6.3 from it), the next upgrade above it (none once NU6.3 is in: NU7
    /// unscheduled), the best as the estimate, the chain from config
    #[test]
    fn lightd_info_takes_branch_and_next_upgrade_from_the_schedule_at_the_served_height() {
        let at = |h: u32| Height::try_from(h).expect("in range");
        let schedule = UpgradeSchedule::compiled(NetworkType::Main).expect("compiled");
        let lightd = |served: u32| Lightd { served: at(served), best: at(3_500_000), schedule };
        let rendered = |served: u32| {
            let info = render_lightd(lightd(served), NetworkType::Main);
            (
                info.chain_name,
                info.sapling_activation_height,
                info.consensus_branch_id,
                (info.block_height, info.estimated_height),
                (info.upgrade_name, info.upgrade_height),
            )
        };
        let main = || "main".to_owned();
        assert_eq!(
            rendered(3_400_000),
            (
                main(),
                419_200,
                "5437f330".to_owned(),
                (3_400_000, 3_500_000),
                ("Nu6.3".to_owned(), 3_428_143)
            )
        );
        assert_eq!(
            rendered(3_428_143),
            (main(), 419_200, "37a5165b".to_owned(), (3_428_143, 3_500_000), (String::new(), 0)),
            "NU6.3 in force, nothing scheduled after"
        );
    }

    /// Over a live publisher, its validator never polled (no holder, ever):
    /// - `UNAVAILABLE` until something is served (no chain yet, then verified but unserved)
    /// - served at 5 under the chain's schedule (every upgrade through NU6.3 from 1): NU6.3's
    ///   branch, the verified best 7 as the estimate, 5 served (one load, no validator)
    #[tokio::test(start_paused = true)]
    async fn lightd_info_answers_once_served_with_no_validator_holding_the_tip() {
        let mut chain = MockChain::regtest();
        let tip_7 = chain.mine_empty(7);
        let node = Arc::new(MockValidator::following(&chain, tip_7));
        let (mut routes, _balancing, _fold) = routes_over(&node);
        let (publisher, nfs) = published(&mut routes);
        let snapshots = routes.snapshots.clone();
        let info = || lightd_info(&snapshots.load(), NetworkType::Main);
        let refused = |why: &str| {
            let refused = info().map_err(|status| (status.code(), status.message().to_owned()));
            refused.err() == Some((tonic::Code::Unavailable, why.to_owned()))
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.clone()));

        let unserved = "the indexes are syncing: nothing served yet";
        assert!(refused(unserved), "no chain yet");
        routes.submit.set_verified(Some(chain.verified(tip_7)));
        rounds("verified, nothing served", || refused(unserved)).await;

        nfs.send_replace(Some(Arc::new(indexed_at(&chain, chain.at(h(5)), Vec::new()))));
        rounds("served", || info().is_ok()).await;
        let expected = LightdInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            vendor: "zaino".to_owned(),
            taddr_support: true,
            chain_name: "main".to_owned(),
            sapling_activation_height: 1,
            consensus_branch_id: "37a5165b".to_owned(),
            estimated_height: 7,
            block_height: 5,
            lightwallet_protocol_version: "v0.5.0".to_owned(),
            ..Default::default()
        };
        assert_eq!(info().expect("served"), expected, "best 7, NU6.3's branch at 5, 5 served");
        assert!(snapshots.load().tips().held_by.is_empty(), "no validator ever held the tip");
        cancel.cancel();
    }

    /// 1,000 subscribers on one thread (a block of wallets), over a live publisher:
    /// - refused without a verified tip
    /// - mempool at the served tip, then each arrival once, in order, same bytes (shared by
    ///   refcount)
    /// - late subscriber = same log; a new best block alone ends nothing; the served tip moving
    ///   ends all in `OK` trailers, and the next load serves the new tip (G5)
    /// - resubscribe opens on the mempool as it now stands
    #[tokio::test(start_paused = true)]
    async fn a_thousand_mempool_streams_share_one_encoded_log_until_the_served_tip_moves() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service as proto;

        const SUBSCRIBERS: usize = 1_000;
        // (bytes, height 0 = unmined) per seed, in the view's (txid) order
        let unmined = |seeds: &[u32]| {
            let mut txs: Vec<_> = seeds.iter().map(|seed| raw_transaction(*seed, 0)).collect();
            txs.sort_by_key(|(txid, _)| *txid);
            txs.into_iter().map(|(_, raw)| (raw, 0)).collect::<Vec<_>>()
        };
        let mut chain = MockChain::regtest();
        let tip_10 = chain.mine_empty(10);
        let tip_11 = chain.mine_empty(1);
        let node = Arc::new(MockValidator::following(&chain, chain.genesis()));
        node.reachable(&Port::ALL, false);
        let (mut routes, balancing, fold) = routes_over(&node);
        let (publisher, nfs) = published(&mut routes);
        let (view, snapshots) = (Arc::clone(&routes.submit), routes.snapshots.clone());
        let reader = view.subscriber();
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.child_token()));
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(fold.run(cancel.child_token()));
        let mut router = dispatch(routes);
        // the header chain's verdict + the NFS's served tip, standing in for both
        let verified = |tip: BlockRef| view.set_verified(Some(chain.verified(tip)));
        let serve = |tip: BlockRef| {
            nfs.send_replace(Some(Arc::new(indexed_at(&chain, tip, Vec::new()))));
        };
        let stream = || framed_request(path::GET_MEMPOOL_STREAM, bytes::Bytes::new());

        let below = router.call(stream()).await.expect("router answers");
        let status = below.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("14")), "no verified tip: UNAVAILABLE");

        node.follow(&chain, tip_10);
        for seed in [1, 2] {
            node.mempool_insert(raw_transaction(seed, 0).1, 1_000);
        }
        node.reachable(&Port::ALL, true);
        verified(tip_10);
        let listed = || reader.current().mempool().is_some_and(|m| m.entries().count() == 2);
        rounds("both listed", listed).await;
        serve(tip_10);
        rounds("10 served", || snapshots.load().tips().served == Some(tip_10)).await;

        let decoded = |chunk: bytes::Bytes| {
            let mut records = Vec::new();
            let mut rest = &chunk[..];
            while !rest.is_empty() {
                let (message, tail) = split_frame(rest).expect("whole frame");
                let record = proto::RawTransaction::decode(message).expect("decodes");
                records.push((record.data.to_vec(), record.height));
                rest = tail;
            }
            records
        };
        let mut subscribers = Vec::with_capacity(SUBSCRIBERS);
        for _ in 0..SUBSCRIBERS {
            let response = router.call(stream()).await.expect("router answers");
            subscribers.push(Box::pin(response.into_body()));
        }
        // Every subscriber's next record: one shared buffer (pointer), decoded once
        async fn next_record<B>(subscribers: &mut [std::pin::Pin<Box<B>>]) -> bytes::Bytes
        where
            B: http_body::Body<Data = bytes::Bytes, Error = Status>,
        {
            let mut first: Option<bytes::Bytes> = None;
            for body in subscribers {
                let frame = body.frame().await.expect("a frame").expect("ok");
                let data = frame.into_data().expect("a record, not the end");
                let shared = first.get_or_insert_with(|| data.clone());
                assert_eq!(data.as_ptr(), shared.as_ptr(), "encoded once, shared by refcount");
            }
            first.expect("subscribers")
        }
        let opening = next_record(&mut subscribers).await;
        assert_eq!(decoded(opening), unmined(&[1, 2]), "the mempool at the block, unmined");

        // a burst of two (one poll: txid order), then one more: each once, in order, to every
        // subscriber
        for seed in [3, 4] {
            node.mempool_insert(raw_transaction(seed, 0).1, 1_000);
        }
        let burst = [next_record(&mut subscribers).await, next_record(&mut subscribers).await];
        let [first, second] = <[_; 2]>::try_from(unmined(&[3, 4])).expect("two");
        assert_eq!(burst.map(decoded), [vec![first.clone()], vec![second.clone()]]);
        node.mempool_insert(raw_transaction(5, 0).1, 1_000);
        assert_eq!(decoded(next_record(&mut subscribers).await), unmined(&[5]));

        // late subscriber: the same opening and the same log, by pointer
        let late = router.call(stream()).await.expect("router answers").into_body();
        let mut late = [Box::pin(late)];
        let late_opening = next_record(&mut late).await;
        assert_eq!(decoded(late_opening), unmined(&[1, 2]), "same block, same opening");
        let late_log = [
            next_record(&mut late).await,
            next_record(&mut late).await,
            next_record(&mut late).await,
        ];
        assert_eq!(late_log.map(decoded), [vec![first], vec![second], unmined(&[5])]);

        // block 11 verified and held, not yet served: every stream stays open
        node.follow(&chain, tip_11);
        rounds("11 held", || reader.current().endpoints()[0].tip() == Some(tip_11)).await;
        verified(tip_11);
        rounds("11 best", || snapshots.load().tips().best == Some(tip_11)).await;
        subscribers.extend(late);
        let open = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            subscribers[0].frame().await
        });
        assert!(open.await.is_err(), "best moved, served did not: still streaming");

        // the NFS serves 11: every stream ends, and the next load serves 11
        serve(tip_11);
        for body in &mut subscribers {
            let ended = body.frame().await.expect("a frame").expect("ok");
            let trailers = ended.into_trailers().expect("the tip ends the stream in trailers");
            assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
            assert!(body.frame().await.is_none(), "nothing after the trailers");
        }
        assert_eq!(snapshots.load().tips().served, Some(tip_11), "ended ⇒ the new tip served");
        let again = router.call(stream()).await.expect("router answers").into_body();
        let reopened = next_record(&mut [Box::pin(again)]).await;
        let now = unmined(&[1, 2, 3, 4, 5]);
        assert_eq!(decoded(reopened), now, "the new tip opens on the mempool as it stands");
        cancel.cancel();
    }

    /// Mempool = 3 mainnet txs (block 2,000,000: Orchard, Sapling, transparent):
    /// - each answer: own slots, own pools (tx left empty dropped), no gap for an excluded suffix
    /// - each pool = what the consensus parse found; each tx parsed once (projection cached)
    #[tokio::test(start_paused = true)]
    async fn get_mempool_tx_parses_each_transaction_once_and_shapes_each_answer() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::types::Transaction;
        use zaino_proto::proto::compact_formats::CompactTx;
        use zaino_proto::proto::service as proto;

        let block: Vec<(Transaction, Vec<u8>)> =
            zaino_source::testing::fixtures::transactions(2_000_000)
                .into_iter()
                .map(|raw| (zaino_source::decode_transaction(&raw).expect("decodes"), raw))
                .collect();
        let pools: [fn(&Transaction) -> bool; 3] = [
            |tx| !tx.orchard.actions.is_empty(),
            |tx| !tx.sapling.outputs.is_empty(),
            |tx| !tx.transparent.inputs.is_empty(),
        ];
        let mut chosen: Vec<(Transaction, Vec<u8>)> = Vec::new();
        for has in pools {
            let fresh = |(tx, _): &&(Transaction, Vec<u8>)| {
                has(tx) && !chosen.iter().any(|(held, _)| held.txid == tx.txid)
            };
            chosen.push(block.iter().find(fresh).expect("block 2,000,000 has one").clone());
        }
        chosen.sort_by_key(|(tx, _)| <[u8; 32]>::from(tx.txid)); // the view's (txid) order
        let txid = |tx: &Transaction| <[u8; 32]>::from(tx.txid);

        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(10);
        let node = Arc::new(MockValidator::following(&chain, tip));
        for (_, raw) in &chosen {
            node.mempool_insert(raw.clone(), 1_000);
        }
        let (mut routes, balancing, fold) = routes_over(&node);
        let (publisher, _nfs) = published(&mut routes);
        let (reader, snapshots) = (routes.submit.subscriber(), routes.snapshots.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.child_token()));
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(fold.run(cancel.child_token()));
        routes.submit.set_verified(Some(chain.verified(tip)));
        let mut router = dispatch(routes);
        let three = || snapshots.load().mempool().is_ok_and(|m| m.entries().count() == 3);
        rounds("three servable", three).await;

        let mut ask = async |request: proto::GetMempoolTxRequest| {
            let request = framed_request(path::GET_MEMPOOL_TX, request.encode_to_vec().into());
            let body = router.call(request).await.expect("router answers").into_body();
            let chunk = body.collect().await.expect("ok").to_bytes();
            let mut answer = Vec::new();
            let mut rest = &chunk[..];
            while !rest.is_empty() {
                let (message, tail) = split_frame(rest).expect("whole frame");
                let tx = CompactTx::decode(message).expect("decodes");
                let parts = (tx.vin.len(), tx.outputs.len(), tx.actions.len());
                answer.push((tx.txid, tx.index, parts));
                rest = tail;
            }
            answer
        };
        // (txid, slot, (transparent inputs, sapling outputs, orchard actions)) as parsed
        let row = |slot: u64, tx: &Transaction, transparent: bool, sapling: bool, orchard: bool| {
            let parts = (
                if transparent { tx.transparent.inputs.len() } else { 0 },
                if sapling { tx.sapling.outputs.len() } else { 0 },
                if orchard { tx.orchard.actions.len() } else { 0 },
            );
            (txid(tx).to_vec(), slot, parts)
        };
        let all = proto::GetMempoolTxRequest { pool_types: vec![1, 2, 3, 4], ..Default::default() };
        let every_pool: Vec<_> =
            (0u64..).zip(&chosen).map(|(slot, (tx, _))| row(slot, tx, true, true, true)).collect();
        assert_eq!(ask(all.clone()).await, every_pool, "every pool, slots 0..3, view order");

        let cached = |reader: &zaino_chainview::ChainViewSubscriber| {
            let pinned = reader.current();
            let mempool = pinned.mempool().expect("tip held");
            let rendered = mempool.entries().all(|entry| {
                entry.projection.get_or_render(|| Err::<bytes::Bytes, ()>(())).is_ok()
            });
            rendered
        };
        assert!(cached(&reader), "every projection rendered by the first request");

        let middle = txid(&chosen[1].0);
        let without_middle = proto::GetMempoolTxRequest {
            exclude_txid_suffixes: vec![middle[28..].to_vec()],
            pool_types: vec![1, 2, 3, 4],
        };
        let (first, last) = (&chosen[0].0, &chosen[2].0);
        let expected = vec![row(0, first, true, true, true), row(1, last, true, true, true)];
        assert_eq!(ask(without_middle).await, expected, "no gap at the excluded");

        let orchard = proto::GetMempoolTxRequest { pool_types: vec![3], ..Default::default() };
        let with_actions: Vec<_> = (0u64..)
            .zip(&chosen)
            .filter(|(_, (tx, _))| !tx.orchard.actions.is_empty())
            .map(|(slot, (tx, _))| row(slot, tx, false, false, true))
            .collect();
        assert!(with_actions.len() < chosen.len(), "the fixture holds a tx with no Orchard action");
        assert_eq!(
            ask(orchard).await,
            with_actions,
            "no component left = dropped, as from a block"
        );
        assert_eq!(ask(all).await, every_pool, "same answer from the cache");
        assert!(cached(&reader), "still the first render (never re-parsed)");
        cancel.cancel();
    }
}
