//! Mempool methods: `GetMempoolTx`, `GetMempoolStream`, both over the `GrpcSnapshot`'s mempool
//! (the chain view's servable entries at the served tip)

use http::Response;
use http_body::Frame;
use tonic::{body::Body, Status};
use zaino_chainview::MempoolEntry;
use zaino_grpc_snapshot::GrpcSnapshot;
use zaino_index_compact_block::project_tx_at;
use zaino_proto::proto::service;

use super::args;
use crate::wire::{self, decode_request, frame, streamed_response, trailers};

/// `GetMempoolTx`: servable mempool minus what the client holds, compacted
///
/// - Materialised, not lazy (CPU over in-memory bytes)
/// - consensus parse once per entry ([`Projection`](zaino_chainview::Projection), slot 0, every
///   pool)
/// - per request: only the slot and the pool selection, over the cached bytes
/// - transaction the selection leaves with no component dropped, as from a block
///   ([`project_tx_at`]), slots stay the listing's
pub(crate) async fn get_mempool_tx<V, B>(
    grpc_snapshot: &GrpcSnapshot<V>,
    body: B,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: service::GetMempoolTxRequest = decode_request(body).await?;
    let pools = args::pools(&request.pool_types)?;

    let mempool = grpc_snapshot.mempool().map_err(wire::unavailable)?;
    let unknown_to_client = mempool.excluding(&request.exclude_txid_suffixes);

    let mut compact_txs = Vec::new();
    for (slot, entry) in unknown_to_client.into_iter().enumerate() {
        let every_pool = entry.projection.get_or_render(|| {
            let parsed = zaino_source::decode_transaction(&entry.raw)
                .map_err(|failed| Status::internal(format!("mempool transaction: {failed}")))?;
            let compact_tx = zaino_index_compact_block::compact_tx(0, &parsed, entry.fee);
            Ok::<_, Status>(bytes::Bytes::from(prost::Message::encode_to_vec(&compact_tx)))
        })?;
        compact_txs.extend(project_tx_at(&every_pool, slot as u64, pools));
    }
    Ok(streamed_response(compact_txs))
}

/// `GetMempoolStream`: the servable mempool as one chunk, then each arrival, closed once the
/// served tip moves (no holder of the verified best = `UNAVAILABLE`, never a silent stream)
///
/// - end ⇒ the next load serves the new tip (`GetLatestBlock` ≥ it: G5)
/// - each record encoded once (first subscriber to reach it)
/// - the rest share it by refcount (a burst ready at once = joined per stream, `Coalesced`)
pub(crate) fn get_mempool_stream<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
) -> Result<Response<Body>, Status> {
    let feed = grpc_snapshot.mempool_stream().map_err(wire::unavailable)?;
    let current_mempool = feed.opening_rendered(|entries| {
        wire::frame_all(&entries.iter().map(unmined).collect::<Vec<_>>())
    });
    let current_mempool =
        (!current_mempool.is_empty()).then(|| Ok::<_, Status>(Frame::data(current_mempool)));

    // feed in the unfold state, not captured (borrowed mutably across an await: `FnMut` can't)
    let arrivals = futures::stream::unfold(Some(feed), move |state| async move {
        let mut feed = state?;
        let Some(arrival) = feed.next().await else {
            return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
        };

        let framed = arrival.rendered(|entry| frame(&unmined(entry)));
        Some((Ok::<_, Status>(Frame::data(framed)), Some(feed)))
    });
    let frames = futures::StreamExt::chain(futures::stream::iter(current_mempool), arrivals);

    Ok(wire::streaming(frames))
}

/// Unmined by construction (wire spells that `height: 0`)
pub(super) fn unmined(entry: &MempoolEntry) -> service::RawTransaction {
    service::RawTransaction { data: entry.raw.clone(), height: 0 }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::HeaderValue;
    use zaino_header_chain::HeaderChainSnapshot;
    use zaino_primitives::types::BlockRef;
    use zaino_source::testing::{raw_transaction, MockChain};
    use zcash_protocol::TxId;

    use crate::service::Method;
    use crate::testing::{
        decode_frames, dispatch, framed_request, indexed_at, published, rounds, routes_over,
    };

    /// 1,000 subscribers on one thread (a block of wallets), over a live publisher:
    ///
    /// - refused without a verified tip
    /// - mempool at the served tip, then each arrival once, in order, same bytes (a lone record
    ///   shared by refcount, a ready burst joined per stream)
    /// - late subscriber = same log
    /// - new best block alone ends nothing
    /// - served tip moving ends all in `OK` trailers, next load serves the new tip (G5)
    /// - resubscribe opens on the mempool as it now stands
    #[tokio::test(start_paused = true)]
    async fn a_thousand_mempool_streams_share_one_encoded_log_until_the_served_tip_moves() {
        use http_body_util::BodyExt as _;
        use tonic::Status;
        use tower::Service as _;
        use zaino_proto::proto::service;

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
        let seeded: Vec<TxId> =
            (1..=5).map(|seed| chain.submit_raw(raw_transaction(seed, 0).1)).collect();
        let node = chain.validator(chain.genesis());
        let hear =
            |seeds: &[usize]| seeds.iter().for_each(|seed| node.hear(&chain, seeded[seed - 1]));
        node.down(true);
        let (mut routes, balancing, merge) = routes_over(&node);
        let (publisher, nfs) = published(&mut routes);
        let (view, grpc_snapshots) =
            (Arc::clone(&routes.chain_view), routes.grpc_snapshots.clone());
        let reader = view.subscriber();
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.child_token()));
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(merge.run(cancel.child_token()));
        let mut router = dispatch(routes);
        // the header chain's verdict + the NFS's served tip, standing in for both
        let verified = |tip: BlockRef| view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let serve = |tip: BlockRef| {
            nfs.send_replace(Some(Arc::new(indexed_at(&chain, tip, Vec::new()))));
        };
        let stream = || framed_request(Method::GetMempoolStream, bytes::Bytes::new());

        let below = router.call(stream()).await.expect("router answers");
        let status = below.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("14")), "no verified tip: UNAVAILABLE");

        node.follow(&chain, tip_10);
        hear(&[1, 2]);
        node.down(false);
        verified(tip_10);
        let listed = || reader.current().mempool().is_some_and(|m| m.entries().count() == 2);
        rounds("both listed", listed).await;
        serve(tip_10);
        rounds("10 served", || grpc_snapshots.load().tips().served == Some(tip_10)).await;

        let decoded = |chunk: bytes::Bytes| {
            let records = decode_frames::<service::RawTransaction>(&chunk);
            let records = records.into_iter().map(|record| (record.data.to_vec(), record.height));
            records.collect::<Vec<_>>()
        };
        let mut subscribers = Vec::with_capacity(SUBSCRIBERS);
        for _ in 0..SUBSCRIBERS {
            let response = router.call(stream()).await.expect("router answers");
            subscribers.push(Box::pin(response.into_body()));
        }
        // - every subscriber's next data frame, the same bytes
        // - `true` = one buffer shared by all
        async fn next_frame<B>(subscribers: &mut [std::pin::Pin<Box<B>>]) -> (bytes::Bytes, bool)
        where
            B: http_body::Body<Data = bytes::Bytes, Error = Status>,
        {
            let mut first: Option<bytes::Bytes> = None;
            let mut shared = true;
            for body in subscribers {
                let frame = body.frame().await.expect("a frame").expect("ok");
                let data = frame.into_data().expect("a record, not the end");
                let seen = first.get_or_insert_with(|| data.clone());
                assert_eq!(data, *seen, "every subscriber, the same records");
                shared &= data.as_ptr() == seen.as_ptr();
            }
            (first.expect("subscribers"), shared)
        }
        let (opening, shared) = next_frame(&mut subscribers).await;
        let opened = (decoded(opening), shared);
        assert_eq!(opened, (unmined(&[1, 2]), true), "the mempool at the block, encoded once");

        // - burst of two (one poll: txid order) = one frame each (joined, `Coalesced`)
        // - then one more = the shared record
        hear(&[3, 4]);
        let (burst, _) = next_frame(&mut subscribers).await;
        assert_eq!(decoded(burst), unmined(&[3, 4]), "in order, once each");
        hear(&[5]);
        let (fifth, shared) = next_frame(&mut subscribers).await;
        assert_eq!((decoded(fifth), shared), (unmined(&[5]), true), "encoded once");

        // late subscriber: the same opening and the same log, all ready = one frame
        let late = router.call(stream()).await.expect("router answers").into_body();
        let mut late = [Box::pin(late)];
        let (log, _) = next_frame(&mut late).await;
        let opening_then_log = [unmined(&[1, 2]), unmined(&[3, 4]), unmined(&[5])].concat();
        assert_eq!(decoded(log), opening_then_log, "same block, same opening, same log");

        // block 11 verified and held, not yet served: every stream stays open
        node.follow(&chain, tip_11);
        rounds("11 held", || reader.current().trusted_validators()[0].tip() == Some(tip_11)).await;
        verified(tip_11);
        rounds("11 best", || grpc_snapshots.load().tips().best == Some(tip_11)).await;
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
        assert_eq!(grpc_snapshots.load().tips().served, Some(tip_11), "ended ⇒ the new tip served");
        let again = router.call(stream()).await.expect("router answers").into_body();
        let (reopened, _) = next_frame(&mut [Box::pin(again)]).await;
        let now = unmined(&[1, 2, 3, 4, 5]);
        assert_eq!(decoded(reopened), now, "the new tip opens on the mempool as it stands");
        cancel.cancel();
    }

    /// Mempool = 3 minted txs on Alice's coinbase: transparent only, + Sapling outputs, + an
    /// Orchard action
    ///
    /// - each answer: own slots, own pools (tx left empty dropped), no gap for an excluded suffix
    /// - each pool = what the consensus parse found
    /// - each tx parsed once (projection cached)
    #[tokio::test(start_paused = true)]
    async fn get_mempool_tx_parses_each_transaction_once_and_shapes_each_answer() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::types::Transaction;
        use zaino_proto::proto::compact_formats::CompactTx;
        use zaino_proto::proto::service;
        use zaino_source::testing::p2pkh;
        use zcash_protocol::ShieldedPool;

        let (alice, zec) = (p2pkh([0xaa; 20]), 100_000_000u64);
        let mut chain = MockChain::regtest();
        let funded =
            chain.mine(|b| b.coinbase(|c| c.pay(&alice, zec).pay(&alice, zec).pay(&alice, zec)));
        let tip = chain.mine_empty(1);
        let node = chain.validator(tip);
        let [a, b, c] = [0, 1, 2].map(|vout| chain.output(funded, 0, vout));
        let deposit = -(zec as i64) + 1_000;
        let submitted = [
            chain.submit(|t| t.spend(a).pay(&alice, zec - 1_000)),
            chain.submit(|t| {
                t.spend(b)
                    .sapling_output(1)
                    .sapling_output(2)
                    .value_balance(ShieldedPool::Sapling, deposit)
            }),
            chain.submit(|t| {
                t.spend(c).orchard_action(1, 1).value_balance(ShieldedPool::Orchard, deposit)
            }),
        ];
        for txid in submitted {
            node.hear(&chain, txid);
        }
        let mut chosen: Vec<(Transaction, Vec<u8>)> = chain
            .mempool()
            .iter()
            .map(|unmined| (unmined.tx.clone(), unmined.bytes.clone()))
            .collect();
        chosen.sort_by_key(|(tx, _)| *tx.txid.as_ref()); // the view's (txid) order
        let txid = |tx: &Transaction| *tx.txid.as_ref();
        let (mut routes, balancing, merge) = routes_over(&node);
        let (publisher, _nfs) = published(&mut routes);
        let (reader, grpc_snapshots) =
            (routes.chain_view.subscriber(), routes.grpc_snapshots.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.child_token()));
        tokio::spawn(balancing.run(cancel.child_token()));
        tokio::spawn(merge.run(cancel.child_token()));
        routes.chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let mut router = dispatch(routes);
        let three = || grpc_snapshots.load().mempool().is_ok_and(|m| m.entries().count() == 3);
        rounds("three servable", three).await;

        let mut ask = async |request: service::GetMempoolTxRequest| {
            let request = framed_request(Method::GetMempoolTx, request.encode_to_vec().into());
            let body = router.call(request).await.expect("router answers").into_body();
            let chunk = body.collect().await.expect("ok").to_bytes();
            let txs = decode_frames::<CompactTx>(&chunk).into_iter();
            let row = |tx: CompactTx| {
                let parts = (tx.vin.len(), tx.outputs.len(), tx.actions.len());
                (tx.txid, tx.index, parts)
            };
            txs.map(row).collect::<Vec<_>>()
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
        let all =
            service::GetMempoolTxRequest { pool_types: vec![1, 2, 3, 4], ..Default::default() };
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
        let without_middle = service::GetMempoolTxRequest {
            exclude_txid_suffixes: vec![middle[28..].to_vec()],
            pool_types: vec![1, 2, 3, 4],
        };
        let (first, last) = (&chosen[0].0, &chosen[2].0);
        let expected = vec![row(0, first, true, true, true), row(1, last, true, true, true)];
        assert_eq!(ask(without_middle).await, expected, "no gap at the excluded");

        let orchard = service::GetMempoolTxRequest { pool_types: vec![3], ..Default::default() };
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
