//! Mempool methods: `GetMempoolTx`, `GetMempoolStream`, both over the `GrpcSnapshot`'s mempool
//! (the chain view's servable entries at the served tip)

use futures::StreamExt as _;
use http::Response;
use tonic::{body::Body, Status};
use zaino_chainview::MempoolEntry;
use zaino_grpc_snapshot::GrpcSnapshot;
use zaino_index_compact_block::project_tx_at;
use zaino_proto::proto::service;

use super::args;
use crate::wire::{self, frame, streamed_response};

/// `GetMempoolTx`: servable mempool minus what the client holds, compacted
///
/// - Materialised, not lazy (CPU over in-memory bytes)
/// - consensus parse once per entry ([`Projection`](zaino_chainview::Projection), slot 0, every
///   pool)
/// - per request: only the slot and the pool selection, over the cached bytes
/// - transaction the selection leaves with no component dropped, as from a block
///   ([`project_tx_at`]), slots stay the listing's
pub(crate) fn get_mempool_tx<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    request: service::GetMempoolTxRequest,
) -> Result<Response<Body>, Status> {
    let pools = args::pools(&request.pool_types)?;

    let mempool = grpc_snapshot.mempool().map_err(wire::unavailable)?;
    let unknown_to_client = mempool.excluding(&request.exclude_txid_suffixes);

    let mut compact_txs = Vec::new();
    for (slot, entry) in (0u64..).zip(unknown_to_client) {
        let every_pool = entry.projection.get_or_render(|| {
            let parsed = zaino_source::decode_transaction(&entry.raw)
                .map_err(|failed| Status::internal(format!("mempool transaction: {failed}")))?;
            let compact_tx = zaino_index_compact_block::compact_tx(0, &parsed, entry.fee);
            Ok::<_, Status>(bytes::Bytes::from(prost::Message::encode_to_vec(&compact_tx)))
        })?;
        compact_txs.extend(project_tx_at(&every_pool, slot, pools));
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
    let current_mempool = (!current_mempool.is_empty()).then_some(Ok(current_mempool));

    // feed in the unfold state, not captured (borrowed mutably across an await: `FnMut` can't)
    let arrivals = futures::stream::unfold(feed, |mut feed| async move {
        let arrival = feed.next().await?;
        Some((Ok(arrival.rendered(|entry| frame(&unmined(entry)))), feed))
    });

    Ok(wire::streaming(futures::stream::iter(current_mempool).chain(arrivals)))
}

/// Unmined by construction (wire spells that `height: 0`)
pub(super) fn unmined(entry: &MempoolEntry) -> service::RawTransaction {
    service::RawTransaction { data: entry.raw.clone(), height: 0 }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use bytes::Bytes;
    use http_body_util::BodyExt as _;
    use tonic::{Code, Status};
    use tower::Service as _;
    use zaino_header_chain::HeaderChainSnapshot;
    use zaino_proto::proto::service;
    use zaino_source::testing::{raw_transaction, within, MockChain};
    use zcash_protocol::TxId;

    use crate::service::Method;
    use crate::testing::{
        answer, decode_frames, framed_request, indexed_at, until, LiveRoutes, MAINNET,
    };

    type Stream = Pin<Box<tonic::body::Body>>;

    /// Seeds of `raw_transaction(seed, 0)` as streamed: unmined, in the view's (txid) order
    fn unmined(seeds: &[u32]) -> Vec<(u32, u64)> {
        let mut seeds = seeds.to_vec();
        seeds.sort_by_key(|seed| raw_transaction(*seed, 0).0);
        seeds.into_iter().map(|seed| (seed, 0)).collect()
    }

    /// Each streamed record as (its `raw_transaction` seed, height)
    fn records(chunk: &Bytes) -> Vec<(u32, u64)> {
        let seed_of = |raw: &[u8]| (1..=5).find(|seed| raw_transaction(*seed, 0).1 == raw);
        let records = decode_frames::<service::RawTransaction>(chunk);
        let seeded = |record: service::RawTransaction| {
            (seed_of(&record.data).expect("a seeded tx"), record.height)
        };
        records.into_iter().map(seeded).collect()
    }

    /// Every stream's next data frame: the same bytes, `true` = one buffer shared by all
    async fn next_frame(streams: &mut [Stream]) -> (Bytes, bool) {
        let mut first: Option<Bytes> = None;
        let mut shared = true;
        for (at, stream) in streams.iter_mut().enumerate() {
            let frame = stream.frame().await.expect("a frame").expect("in-band");
            let data = frame.into_data().unwrap_or_else(|end| panic!("stream {at} ended: {end:?}"));
            let seen = first.get_or_insert_with(|| data.clone());
            assert_eq!(records(&data), records(seen), "stream {at} vs stream 0: same records");
            shared &= data.as_ptr() == seen.as_ptr();
        }
        (first.expect("streams"), shared)
    }

    /// 1,000 subscribers on one thread (a block of wallets): each record encoded once, shared by
    /// refcount (a ready burst joined per stream), a late subscriber served the same log
    #[tokio::test(start_paused = true)]
    async fn a_thousand_mempool_streams_share_one_encoded_log() {
        const SUBSCRIBERS: usize = 1_000;
        let mut chain = MockChain::regtest();
        let tip_10 = chain.mine_empty(10);
        let seeded: Vec<TxId> =
            (1..=5).map(|seed| chain.submit_raw(raw_transaction(seed, 0).1)).collect();
        let node = chain.validator(tip_10);
        let hear =
            |seeds: &[usize]| seeds.iter().for_each(|&seed| node.hear(&chain, seeded[seed - 1]));
        let live = LiveRoutes::over(&node, MAINNET, &[]);
        let mut router = live.router.clone();
        let stream = || framed_request(Method::GetMempoolStream, Bytes::new());
        hear(&[1, 2]);
        live.chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip_10)));
        let listed = || {
            live.chain_view
                .subscriber()
                .current()
                .mempool()
                .is_some_and(|m| m.entries().count() == 2)
        };
        within("txs 1 + 2 listed at 10", until(listed)).await;
        live.nfs.send_replace(Some(Arc::new(indexed_at(&chain, tip_10, Vec::new()))));
        within("10 served", until(|| live.grpc_snapshots.load().tips().served == Some(tip_10)))
            .await;
        let mut streams = Vec::with_capacity(SUBSCRIBERS);
        for _ in 0..SUBSCRIBERS {
            streams.push(Box::pin(router.call(stream()).await.expect("infallible").into_body()));
        }

        let (opening, shared) = next_frame(&mut streams).await;
        assert_eq!(
            (records(&opening), shared),
            (unmined(&[1, 2]), true),
            "opening at 10, encoded once"
        );
        hear(&[3, 4]);
        let (burst, _) = next_frame(&mut streams).await;
        assert_eq!(
            records(&burst),
            unmined(&[3, 4]),
            "burst of two (one poll): one frame, txid order"
        );
        hear(&[5]);
        let (fifth, shared) = next_frame(&mut streams).await;
        assert_eq!((records(&fifth), shared), (unmined(&[5]), true), "lone arrival, encoded once");

        let late = Box::pin(router.call(stream()).await.expect("infallible").into_body());
        let (log, _) = next_frame(&mut [late]).await;
        let opening_then_log = [unmined(&[1, 2]), unmined(&[3, 4]), unmined(&[5])].concat();
        assert_eq!(records(&log), opening_then_log, "late: same opening + log, one frame");
    }

    /// - nothing verified = refused before any record
    /// - G5: a stream's end ⇒ the next load serves the new tip
    #[tokio::test(start_paused = true)]
    async fn mempool_streams_end_when_the_served_tip_moves_not_when_only_the_best_does() {
        let mut chain = MockChain::regtest();
        let tip_10 = chain.mine_empty(10);
        let tip_11 = chain.mine_empty(1);
        let seeded: Vec<TxId> =
            (1..=2).map(|seed| chain.submit_raw(raw_transaction(seed, 0).1)).collect();
        let node = chain.validator(tip_10);
        let live = LiveRoutes::over(&node, MAINNET, &[]);
        let mut router = live.router.clone();
        let stream = || framed_request(Method::GetMempoolStream, Bytes::new());
        let verified = |tip| live.chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let serve =
            |tip| live.nfs.send_replace(Some(Arc::new(indexed_at(&chain, tip, Vec::new()))));
        let tips = || live.grpc_snapshots.load().tips();
        let (chunks, status) = answer(router.call(stream()).await.expect("infallible")).await;
        let refusal = (status.code(), chunks);
        assert_eq!(refusal, (Code::Unavailable, Vec::new()), "nothing verified: {status:?}");
        node.hear(&chain, seeded[0]);
        verified(tip_10);
        within(
            "tx 1 listed at 10",
            until(|| live.chain_view.subscriber().current().mempool().is_some()),
        )
        .await;
        serve(tip_10);
        within("10 served", until(|| tips().served == Some(tip_10))).await;
        let mut streams = Vec::new();
        for _ in 0..3 {
            streams.push(Box::pin(router.call(stream()).await.expect("infallible").into_body()));
        }
        assert_eq!(records(&next_frame(&mut streams).await.0), unmined(&[1]), "opening at 10");

        node.follow(&chain, tip_11);
        verified(tip_11);
        within("11 best, 10 still served", until(|| tips().best == Some(tip_11))).await;
        node.hear(&chain, seeded[1]);
        let (arrival, _) = next_frame(&mut streams).await;
        assert_eq!(
            records(&arrival),
            unmined(&[2]),
            "best 11, served 10: still streaming arrivals"
        );

        serve(tip_11);
        for (at, stream) in streams.iter_mut().enumerate() {
            let end = stream.frame().await.expect("a frame").expect("in-band").into_trailers();
            let status = end.ok().and_then(|trailers| Status::from_header_map(&trailers));
            assert_eq!(
                status.as_ref().map(Status::code),
                Some(Code::Ok),
                "stream {at}: {status:?}"
            );
            assert!(stream.frame().await.is_none(), "stream {at}: a frame after its trailers");
        }
        assert_eq!(tips().served, Some(tip_11), "ended ⇒ the next load serves 11");
        let reopened = Box::pin(router.call(stream()).await.expect("infallible").into_body());
        let (opening, _) = next_frame(&mut [reopened]).await;
        assert_eq!(records(&opening), unmined(&[1, 2]), "reopened at 11: the mempool as it stands");
    }

    /// Every projection cached by the first ask; each answer its own slots + pools, a tx the
    /// selection empties dropped (as from a block), no gap for an excluded suffix
    #[tokio::test(start_paused = true)]
    async fn get_mempool_tx_caches_every_projection_on_its_first_ask_and_shapes_each_answer() {
        use prost::Message as _;
        use zaino_proto::proto::compact_formats::CompactTx;
        use zaino_source::testing::p2pkh;
        use zcash_protocol::ShieldedPool;

        let (alice, zec) = (p2pkh([0xaa; 20]), 100_000_000u64);
        let mut chain = MockChain::regtest();
        let funded =
            chain.mine(|b| b.coinbase(|c| c.pay(&alice, zec).pay(&alice, zec).pay(&alice, zec)));
        let tip = chain.mine_empty(1);
        let node = chain.validator(tip);
        let [a, b, c] = [0, 1, 2].map(|vout| chain.output(funded, 0, vout));
        let deposit = 1_000 - i64::try_from(zec).expect("1 ZEC fits i64");
        let submitted = [
            ("transparent", chain.submit(|t| t.spend(a).pay(&alice, zec - 1_000))),
            (
                "2 sapling outputs",
                chain.submit(|t| {
                    t.spend(b)
                        .sapling_output(1)
                        .sapling_output(2)
                        .value_balance(ShieldedPool::Sapling, deposit)
                }),
            ),
            (
                "1 orchard action",
                chain.submit(|t| {
                    t.spend(c).orchard_action(1, 1).value_balance(ShieldedPool::Orchard, deposit)
                }),
            ),
        ];
        for (_, txid) in submitted {
            node.hear(&chain, txid);
        }
        let mut in_view_order = submitted;
        in_view_order.sort_by_key(|(_, txid)| *txid.as_ref());
        let live = LiveRoutes::over(&node, MAINNET, &[]);
        let mut router = live.router.clone();
        live.chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip)));
        let servable =
            || live.grpc_snapshots.load().mempool().is_ok_and(|m| m.entries().count() == 3);
        within("3 servable at the tip", until(servable)).await;
        // per answered tx: (slot, which, (transparent ins, sapling outputs, orchard actions))
        let mut ask = async |pool_types: Vec<i32>, exclude_txid_suffixes: Vec<Vec<u8>>| {
            let request = service::GetMempoolTxRequest { exclude_txid_suffixes, pool_types };
            let response = router
                .call(framed_request(Method::GetMempoolTx, request.encode_to_vec().into()))
                .await;
            let (chunks, status) = answer(response.expect("infallible")).await;
            assert_eq!(status.code(), Code::Ok, "{status:?}");
            let which = |txid: &[u8]| {
                in_view_order.iter().find(|(_, id)| id.as_ref() == txid).map(|(which, _)| *which)
            };
            let row = |tx: CompactTx| {
                (tx.index, which(&tx.txid), (tx.vin.len(), tx.outputs.len(), tx.actions.len()))
            };
            decode_frames::<CompactTx>(&chunks.concat()).into_iter().map(row).collect::<Vec<_>>()
        };
        let unrendered = || {
            let pinned = live.chain_view.subscriber().current();
            let mempool = pinned.mempool().expect("tip held");
            let unrendered = mempool
                .entries()
                .filter(|e| e.projection.get_or_render(|| Err::<Bytes, ()>(())).is_err());
            unrendered.map(|entry| entry.txid).collect::<Vec<_>>()
        };
        let every_pool = vec![1, 2, 3, 4];
        let parts = |which: &str| match which {
            "transparent" => (1, 0, 0),
            "2 sapling outputs" => (1, 2, 0),
            _ => (1, 0, 1),
        };
        let all: Vec<_> = (0u64..)
            .zip(in_view_order)
            .map(|(slot, (which, _))| (slot, Some(which), parts(which)))
            .collect();

        assert_eq!(
            ask(every_pool.clone(), Vec::new()).await,
            all,
            "every pool, slots 0..3, view order"
        );
        assert_eq!(unrendered(), Vec::new(), "every projection rendered by the first request");

        let middle = in_view_order[1].1.as_ref()[28..].to_vec();
        let without_middle = vec![(0, all[0].1, all[0].2), (1, all[2].1, all[2].2)];
        assert_eq!(
            ask(every_pool.clone(), vec![middle]).await,
            without_middle,
            "no gap at the excluded"
        );

        let orchard_slot =
            all.iter().position(|row| row.1 == Some("1 orchard action")).expect("listed");
        let orchard_slot = u64::try_from(orchard_slot).expect("a slot of 3");
        let orchard_only = vec![(orchard_slot, Some("1 orchard action"), (0, 0, 1))];
        assert_eq!(
            ask(vec![3], Vec::new()).await,
            orchard_only,
            "[ORCHARD]: emptied txs dropped, slots kept"
        );
        assert_eq!(ask(every_pool, Vec::new()).await, all, "same answer from the cache");
    }
}
