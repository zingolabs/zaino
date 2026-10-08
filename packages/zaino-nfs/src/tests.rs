//! [`Nfs`] end to end: every real fold, `SimFs` stores, mock validators, a test-only committer per
//! index (`nfs.md` §10)
//!
//! - Oracle = each index's own fold from genesis along best, into fresh stores
//! - Every snapshot seen: tip on its best; `at` of every mined block (served, side, root): each
//!   index = the oracle at its view's tip, that tip = the block's (R12: a durable-only view ahead)
//! - Final stream, per index and run: each height once, ascending, final, folded once folded

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_index_compact_block::CompactBlockReader;
use zaino_index_transparent_address::TransparentAddressReader;
use zaino_index_tree_state::TreeStateReader;
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_internal_value_balance::ValueBalanceReader;
use zaino_persistence::{
    fs::SimFs, Changes, DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema, Store, View,
    Width,
};
use zaino_primitives::testing::{h, p2pkh, MockChain};
use zaino_primitives::types::{Block, BlockHash, OutPoint, ReorgDepth};
use zaino_source::testing::{Lie, MockValidator};
use zaino_traffic::{Limits, Trusted};
use zcash_protocol::consensus::NetworkType;

use super::*;

const NETWORK: NetworkType = NetworkType::Regtest;
/// Subscribe order = fold order: (index, commit lag s, last height run 0 commits before its crash)
const INDEXES: [(IndexKind, u64, u32); 5] = [
    (IndexKind::ValueBalance, 0, 17),
    (IndexKind::CompactBlock, 1, 16),
    (IndexKind::BlockHash, 2, 15),
    (IndexKind::TreeState, 3, 14),
    (IndexKind::TransparentAddress, 1, 16),
];

/// Every record and row, table by table (engine-agnostic equality)
type Tables = Vec<Vec<Vec<u8>>>;

/// `kind`'s store schema, as zainod opens it
pub(crate) fn schema(kind: IndexKind) -> Schema {
    use zaino_index_compact_block as compact_block;
    use zaino_index_transparent_address as transparent_address;
    use zaino_index_tree_state as tree_state;
    use zaino_internal_block_hash_to_height as block_hash;
    use zaino_internal_value_balance as value_balance;

    let (format, tables) = match kind {
        IndexKind::ValueBalance => (value_balance::FORMAT, value_balance::TABLES),
        IndexKind::CompactBlock => (compact_block::FORMAT, compact_block::TABLES),
        IndexKind::BlockHash => (block_hash::FORMAT, block_hash::TABLES),
        IndexKind::TreeState => (tree_state::FORMAT, tree_state::TABLES),
        IndexKind::TransparentAddress => (transparent_address::FORMAT, transparent_address::TABLES),
        IndexKind::HeaderChain => panic!("not an NFS index"),
    };
    Schema::new(kind, format, NETWORK, tables)
}

/// `kind`'s own fold of `block` onto `parent` into `out` (`value_balance` = a state at or past
/// the block's parent: compact-block's fees)
fn own_fold(
    kind: IndexKind,
    parent: impl SequenceRead + MapRead,
    value_balance: impl MapRead,
    block: &Block,
    out: &mut Changes,
) {
    match kind {
        IndexKind::ValueBalance => {
            let parent = ValueBalanceReader::new(parent);
            zaino_internal_value_balance::fold(&parent, block, out).expect("prevouts held");
        }
        IndexKind::CompactBlock => {
            let fees = ValueBalanceReader::new(value_balance);
            let fees = zaino_internal_value_balance::fees(&fees, &[block]).expect("held");
            let parent = CompactBlockReader::new(parent);
            zaino_index_compact_block::fold(&parent, block, &fees[0], out).expect("sizes in range");
        }
        IndexKind::BlockHash => {
            let parent = BlockHashReader::new(parent);
            zaino_internal_block_hash_to_height::fold(&parent, block, out);
        }
        IndexKind::TreeState => {
            let parent = TreeStateReader::new(parent);
            zaino_index_tree_state::fold(&parent, block, out).expect("canonical commitments");
        }
        IndexKind::TransparentAddress => {
            let parent = TransparentAddressReader::new(parent);
            zaino_index_transparent_address::fold(&parent, block, out);
        }
        IndexKind::HeaderChain => panic!("not an NFS index"),
    }
}

/// Every record and row of `view`, table by table
fn tables(view: &(impl SequenceRead + MapRead)) -> Tables {
    let schema = *view.schema();
    let sequences = schema.sequences().iter().map(|&table| {
        let table = view.sequence(table);
        let records = table.records(0..table.count());
        records.iter().map(|record| record.to_vec()).collect()
    });
    let maps = schema.maps().iter().map(|&table| {
        let Width::Fixed(key) = table.key else { panic!("fixed-width keys") };
        let past = vec![0xff; key.get() as usize + 1];
        let rows = view.map(table).range(&[], &past, usize::MAX).expect("under the limit");
        rows.iter().map(|(key, value)| [&key[..], &value[..]].concat()).collect()
    });
    sequences.chain(maps).collect()
}

/// Every index folded from genesis through `path` by its own fold, into fresh stores
fn oracle(path: &[Arc<Block>]) -> Vec<(IndexKind, Tables)> {
    let engine = DiskEngine::new(SimFs::new());
    let open = |kind: IndexKind| engine.open(Path::new(kind.name()), &schema(kind));
    let mut stores: Vec<(IndexKind, DiskStore)> =
        INDEXES.iter().map(|&(kind, ..)| (kind, open(kind).expect("fresh store"))).collect();
    for block in path {
        let value_balance = stores[0].1.staged();
        for (kind, store) in &mut stores {
            let mut out = store.changes(block.at());
            own_fold(*kind, store.staged(), value_balance.clone(), block, &mut out);
            store.apply(out);
        }
    }
    let mut folded = Vec::new();
    for (kind, mut store) in stores {
        store.commit().expect("SimFs commit");
        folded.push((kind, tables(&store.view())));
    }
    folded
}

/// Test-only writer: each `Final` applied (folded for it, else its own fold) and committed after
/// `lag`
///
/// - A height already held skipped (restart: an index ahead of the root)
/// - Folded without it (it lagged when the NFS folded): its own fold
/// - Past `crash`: received, never committed (the crashed process's lost steps)
/// - Returns every step received: `(block, folded)`
async fn commit(
    kind: IndexKind,
    store: &mut DiskStore,
    mut blocks: Subscription<Final>,
    committed: &watch::Sender<DiskView>,
    mut value_balance: watch::Receiver<DiskView>,
    lag: Duration,
    crash: Option<Height>,
) -> Vec<(BlockRef, bool)> {
    let mut received = Vec::new();
    loop {
        let Step::Apply { data, .. } = blocks.next().await else { return received };
        let header = data.block.header();
        let at = BlockRef { hash: header.hash, height: header.height };
        received.push((at, data.folds.is_some()));
        let held = Some(at.height) <= store.view().tip().map(|tip| tip.height);
        if held || crash.is_some_and(|crash| at.height > crash) {
            continue;
        }
        let changes = match data.folds.as_deref().and_then(|folds| folds.get(kind)) {
            Some(changes) => changes.clone(),
            None => {
                let below = at.height.checked_sub(1);
                let fees = value_balance.wait_for(|view| view.tip().map(|tip| tip.height) >= below);
                let fees = fees.await.expect("value_balance committer alive").clone();
                let mut out = store.changes(at);
                own_fold(kind, store.view(), fees, &data.block, &mut out);
                out
            }
        };
        store.apply(changes);
        tokio::time::sleep(lag).await;
        store.commit().expect("SimFs commit");
        committed.send_replace(store.view());
    }
}

/// - N4: tip on its chain's best = the lowest view served (a root snapshot: its lowest joined)
/// - G7: `at` of every mined block, served included: `Some` iff folded or the root; branch = the
///   block's path vs the chain's; each index served reads through the block, or (durable at or
///   past it) its durable tip alone (R12); either = the oracle there
/// - J3: an index not served = enabled and `syncing` (lagging, or joined after the fold)
fn verify(
    snapshot: &Indexed<DiskView>,
    blocks: &MockChain,
    mined: &[BlockHash],
    oracles: &mut HashMap<BlockHash, Vec<(IndexKind, Tables)>>,
    context: &str,
) {
    let (chain, tip) = (snapshot.chain(), snapshot.served().tip());
    assert_eq!(chain.hash_at(tip.height), Some(tip.hash), "{context}: N4 tip {tip:?} off best");
    assert_eq!(snapshot.served().branch(), Branch::Best, "{context}: N4 served on the best");
    let durable: Vec<(IndexKind, Option<BlockRef>)> = snapshot.durable().collect();
    let views = snapshot.served().views();
    let lowest = durable.iter().filter_map(|(kind, _)| views.view(*kind)?.tip());
    let lowest = lowest.map(|at| at.height).min();
    assert_eq!(lowest, Some(tip.height), "{context}: N4 the tip = the lowest view served");

    for hash in mined {
        let Some(at) = snapshot.at(hash) else {
            let held =
                snapshot.folded(hash) || snapshot.root().is_some_and(|root| root.hash == *hash);
            assert!(!held, "{context}: G7 at({hash:?}) = None for a folded block or the root");
            continue;
        };
        let block = at.tip();
        let path = blocks.blocks(blocks.block(*hash).at());
        let path: Vec<BlockHash> = path.iter().map(|b| b.header().hash).collect();
        let shared = (0..path.len()).take_while(|&h| chain.hash_at(height(h)) == Some(path[h]));
        let branch = match shared.count() {
            all if all == path.len() => Branch::Best,
            shared => Branch::Side {
                from: BlockRef { hash: path[shared - 1], height: height(shared - 1) },
            },
        };
        assert_eq!((block.hash, at.branch()), (*hash, branch), "{context}: G7 at({hash:?})");
        for (kind, durable) in &durable {
            let name = kind.name();
            let through = durable.filter(|durable| durable.height >= block.height).unwrap_or(block);
            let Some(view) = at.views().view(*kind) else {
                let syncing = at.views().syncing(*kind);
                assert!(
                    syncing,
                    "{context}: J3 {name} at {block:?}: enabled, not served = syncing"
                );
                continue;
            };
            assert_eq!(view.tip(), Some(through), "{context}: G7 {name} at {block:?}");
            let expected =
                oracles.entry(through.hash).or_insert_with(|| oracle(&blocks.blocks(through)));
            let (_, expected) =
                expected.iter().find(|(each, _)| each == kind).expect("every index");
            let got = tables(&view);
            assert!(got == *expected, "{context}: N6 {name} at {through:?} != folded from genesis");
        }
    }
}

fn height(h: usize) -> Height {
    Height::try_from(h as u32).expect("small chain")
}

/// Block tree (mined up front), then two NFS runs over the same stores, each move settled (every
/// index durable through final, served tip = best):
///
/// - Run 0: A 1..=12 (final 9: bulk, then tip), B 10..=13 off A9 (longer, heavier), C13 off B12
///   (same height), D12 off B11 (retreat), E 13..=16 (final 13: folded sends), E 17..=20 (final
///   17: indexes crash at 14..=17)
/// - Run 1: restart from those tips (apart), E 21..=24 (final 21)
/// - Each block: a coinbase paying 10 000, + a spend of its parent's coinbase (fee 1 000) with one
///   sapling output and one orchard action
/// - Bodies through the balancer: an honest member + a `Lie::Poisoned` liar (each lie reported,
///   re-asked; never folded: the oracle would differ)
#[tokio::test(start_paused = true)]
async fn every_snapshot_answers_like_folding_from_genesis_through_reorgs_finality_and_a_restart() {
    let miner = p2pkh([0xad; 20]);
    let mut blocks =
        MockChain::regtest().varied_work().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    let mut tag = 0u32;
    // `count` blocks on `parent`, the first outweighing the best when `outweigh`
    let mut mine = |blocks: &mut MockChain, parent: BlockRef, count: u32, outweigh: bool| {
        let mut mined = Vec::new();
        let mut tip = parent;
        for at in 0..count {
            tag += 1;
            let funding = OutPoint { txid: blocks.block(tip.hash).transactions()[0].txid, vout: 0 };
            let mut nullifier = [0x0f; 32];
            nullifier[..4].copy_from_slice(&tag.to_le_bytes());
            let branch = blocks.branch(tip);
            let branch = if outweigh && at == 0 { branch.outweigh() } else { branch };
            tip = branch
                .mine(|b| {
                    b.coinbase(|c| c.pay(&miner, 10_000)).tx(|t| {
                        t.spend(funding)
                            .pay(&miner, 9_000)
                            .fee(1_000)
                            .sapling_output(tag)
                            .orchard_action(nullifier, tag)
                    })
                })
                .tip();
            mined.push(Arc::clone(blocks.block(tip.hash)));
        }
        mined
    };
    let hash = |block: &Arc<Block>| block.header().hash;
    let genesis = blocks.genesis();
    let a = [blocks.blocks(genesis), mine(&mut blocks, genesis, 12, false)].concat();
    let b = mine(&mut blocks, a[9].at(), 4, true);
    let c13 = mine(&mut blocks, b[2].at(), 1, true).remove(0);
    let d12 = mine(&mut blocks, b[1].at(), 1, true).remove(0);
    let e = mine(&mut blocks, d12.at(), 12, false);
    let blocks = blocks;
    let tree = [&a[..], &b, &[c13.clone(), d12.clone()], &e];
    let mined: Vec<BlockHash> = tree.iter().flat_map(|run| run.iter().map(hash)).collect();
    type Added<'a> = (&'a [Arc<Block>], bool);
    // (headers added, finalize)
    let runs: [&[Added]; 2] = [
        &[
            (&a[1..], true),
            (&b, false),
            (std::slice::from_ref(&c13), false),
            (std::slice::from_ref(&d12), false),
            (&e[..4], true),
            (&e[4..8], true),
        ],
        &[(&[], false), (&e[8..], true)],
    ];

    let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
    let mut headers = blocks.header_chain(depth);
    let members = [None, Some(Lie::Poisoned)].map(|lie| {
        let member = MockValidator::following(&blocks, genesis);
        member.lie(lie);
        Arc::new(member)
    });
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted =
        members.iter().map(|member| Trusted { source: Arc::clone(member), priority: 0, limits });
    let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let params = ChainParams::of(&blocks, genesis);
    let engine = DiskEngine::new(SimFs::new());
    let mut indexes: Vec<(IndexKind, DiskStore, watch::Sender<DiskView>)> = INDEXES
        .iter()
        .map(|&(kind, ..)| {
            let store = engine.open(Path::new(kind.name()), &schema(kind));
            let store = store.expect("fresh store");
            let committed = watch::channel(store.view()).0;
            (kind, store, committed)
        })
        .collect();
    let mut oracles = HashMap::new();
    let queue = NonZeroUsize::new(1 << 24).expect("nonzero");
    let lookahead = NonZeroUsize::new(4).expect("nonzero");

    for (run, moves) in runs.iter().enumerate() {
        let mut nfs = Nfs::new(verified_rx.clone(), balancer.clone(), params, lookahead);
        let value_balance = indexes[0].2.subscribe();
        let durable: Vec<watch::Receiver<DiskView>> =
            indexes.iter().map(|(_, _, committed)| committed.subscribe()).collect();
        let tips: Vec<Option<Height>> =
            durable.iter().map(|view| view.borrow().tip().map(|tip| tip.height)).collect();
        let apart = tips.iter().any(|tip| *tip != tips[0]);
        assert_eq!(apart, run == 1, "run {run}: restarted with indexes apart {tips:?}");
        let root = tips.iter().min().copied().flatten();
        let crashes: Vec<Option<Height>> = INDEXES
            .iter()
            .map(|&(.., crash)| (run == 0).then(|| Height::try_from(crash).expect("small chain")))
            .collect();
        let committers: Vec<_> = indexes
            .drain(..)
            .zip(INDEXES)
            .zip(crashes.clone())
            .map(|(((kind, mut store, committed), (_, lag, _)), crash)| {
                let blocks = nfs.subscribe(kind, committed.subscribe(), queue);
                let value_balance = value_balance.clone();
                tokio::spawn(async move {
                    let lag = Duration::from_secs(lag);
                    let received =
                        commit(kind, &mut store, blocks, &committed, value_balance, lag, crash);
                    let received = received.await;
                    (kind, store, committed, received)
                })
            })
            .collect();
        let (mut indexed, progress) = (nfs.indexed(), nfs.progress());
        let cancel = CancellationToken::new();
        let mut driver = tokio::spawn(nfs.run(cancel.clone()));

        for (at, &(added, finalize)) in moves.iter().enumerate() {
            let context = format!("run {run} move {at}");
            insert(&mut headers, added).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary).expect("in-memory store");
            }
            if let Some(tip) = added.last() {
                members.iter().for_each(|member| member.follow(&blocks, tip.at()));
            }
            verified.send_replace(headers.verified().map(Arc::new));
            let best = headers.best().expect("verified").block;
            let final_height = headers.final_tip().map(|tip| tip.height);
            let durable_heights = || -> Vec<Option<Height>> {
                durable.iter().map(|view| view.borrow().tip().map(|tip| tip.height)).collect()
            };
            let settled_heights: Vec<Option<Height>> =
                crashes.iter().map(|crash| final_height.min(crash.or(final_height))).collect();
            let mut seen: Option<Arc<Indexed<DiskView>>> = None;
            let settled = async {
                loop {
                    let new = |latest: &Arc<_>| {
                        !seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, latest))
                    };
                    let latest = indexed.borrow_and_update().clone();
                    if let Some(latest) = latest.filter(new) {
                        verify(&latest, &blocks, &mined, &mut oracles, &context);
                        seen = Some(latest);
                    }
                    let durable = durable_heights() == settled_heights;
                    if durable && seen.as_ref().is_some_and(|seen| seen.served().tip() == best) {
                        return;
                    }
                    tokio::select! {
                        Ok(()) = indexed.changed() => {}
                        stopped = &mut driver => match stopped {
                            Err(join) => std::panic::resume_unwind(join.into_panic()),
                            Ok(result) => panic!("{context}: driver stopped: {result:?}"),
                        },
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
            };
            let day = Duration::from_secs(86_400);
            if tokio::time::timeout(day, settled).await.is_err() {
                let (durable, settled) = (durable_heights(), settled_heights);
                panic!("{context}: never settled: {best:?}, durable {durable:?} != {settled:?}");
            }
            let handed = progress.handed().expect("blocks handed");
            assert!(handed <= best.height, "{context}: handed {handed:?} past best {best:?}");
        }

        cancel.cancel();
        driver.await.expect("driver task").expect("cancel = clean stop");
        let chain = headers.verified().expect("verified");
        let final_height = chain.final_tip().map(|tip| tip.height);
        for committer in committers {
            let (kind, store, committed, received) = committer.await.expect("committer task");
            let name = kind.name();
            let heights: Vec<Height> = received.iter().map(|(at, _)| at.height).collect();
            let from = root.map_or(Height::GENESIS, Height::next);
            let expected: Vec<Height> = from.up_to(final_height.expect("final")).collect();
            assert_eq!(heights, expected, "run {run}: N5 {name} each height once, through final");
            for (at, _) in &received {
                let best = chain.hash_at(at.height) == Some(at.hash);
                assert!(best, "run {run}: N5 {name} {at:?} off the final chain");
            }
            let folded: Vec<bool> = received.iter().map(|(_, folded)| *folded).collect();
            assert!(folded.is_sorted(), "run {run}: {name} unfolded after folded {folded:?}");
            assert!(
                folded.contains(&false) && folded.contains(&true),
                "run {run}: {name} bulk then tip {folded:?}"
            );
            indexes.push((kind, store, committed));
        }
    }
    stop_balancing.cancel();
}

/// - Run 0: four indexes synced through A8 (final 5)
/// - Down: A9 arrives (final 6, past their root A5)
/// - Run 1: tree-state enabled on a fresh store, its writer stalled (1-byte queue, nothing popped:
///   the stream held at genesis, as a slow bulk sync holds it)
/// - Run 1: A 10..=16 one at a time (final = best - 3), then tree-state released, all settled
/// - The four: served at best after each block, tree-state still at nothing durable (they never
///   wait for it)
/// - Served tip never back
/// - Tree-state: absent (`syncing`) from every snapshot until durable at the four's root (A5),
///   then served in every later one
/// - Every snapshot = folding from genesis (`verify`); each final stream = 0..=final once,
///   never unfolded after folded
/// - Settled: every index durable through final, served at best, tree-state included
#[tokio::test(start_paused = true)]
async fn an_index_enabled_late_syncs_alone_while_the_others_serve_the_tip_then_joins() {
    let miner = p2pkh([0xad; 20]);
    let mut blocks = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    for tag in 1..=16u32 {
        let funding = blocks.block(blocks.tip().hash).transactions()[0].txid;
        let mut nullifier = [0x0f; 32];
        nullifier[..4].copy_from_slice(&tag.to_le_bytes());
        blocks.mine(|b| {
            b.coinbase(|c| c.pay(&miner, 10_000)).tx(|t| {
                t.spend(OutPoint { txid: funding, vout: 0 })
                    .pay(&miner, 9_000)
                    .fee(1_000)
                    .sapling_output(tag)
                    .orchard_action(nullifier, tag)
            })
        });
    }
    let blocks = blocks;
    let (genesis, a16) = (blocks.genesis(), blocks.tip());
    let a = blocks.blocks(a16);
    let mined: Vec<BlockHash> = a.iter().map(|block| block.header().hash).collect();
    let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
    let mut headers = blocks.header_chain(depth);
    let member = Arc::new(MockValidator::following(&blocks, genesis));
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&member), priority: 0, limits }];
    let (balancer, balancing) = TrafficBalancer::new(trusted, None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let params = ChainParams::of(&blocks, a16);
    let engine = DiskEngine::new(SimFs::new());
    let open = |kind: IndexKind| {
        let store = engine.open(Path::new(kind.name()), &schema(kind)).expect("fresh store");
        let committed = watch::channel(store.view()).0;
        (kind, store, committed)
    };
    let four = [
        IndexKind::ValueBalance,
        IndexKind::CompactBlock,
        IndexKind::BlockHash,
        IndexKind::TransparentAddress,
    ];
    let mut indexes: Vec<(IndexKind, DiskStore, watch::Sender<DiskView>)> =
        four.into_iter().map(open).collect();
    let mut oracles = HashMap::new();
    let lookahead = NonZeroUsize::new(4).expect("nonzero");
    let root = height(5);
    let mut add = |added: &[Arc<Block>]| {
        insert(&mut headers, added).expect("valid headers");
        if let Some(boundary) = headers.finalizable() {
            headers.finalize(boundary).expect("in-memory store");
        }
        if let Some(tip) = added.last() {
            member.follow(&blocks, tip.at());
        }
        verified.send_replace(headers.verified().map(Arc::new));
        (headers.best().expect("verified").block, headers.final_tip().map(|tip| tip.height))
    };
    let (release, released) = watch::channel(false);

    for run in 0..2 {
        if run == 1 {
            add(&a[9..=9]);
            indexes.insert(3, open(IndexKind::TreeState));
        }
        let mut nfs = Nfs::new(verified_rx.clone(), balancer.clone(), params, lookahead);
        let value_balance = indexes[0].2.subscribe();
        let durable: Vec<watch::Receiver<DiskView>> =
            indexes.iter().map(|(_, _, committed)| committed.subscribe()).collect();
        let committers: Vec<_> = indexes
            .drain(..)
            .map(|(kind, mut store, committed)| {
                let late = kind == IndexKind::TreeState;
                let queue = match late {
                    true => NonZeroUsize::MIN,
                    false => NonZeroUsize::new(1 << 24).expect("nonzero"),
                };
                let blocks = nfs.subscribe(kind, committed.subscribe(), queue);
                let value_balance = value_balance.clone();
                let mut released = released.clone();
                tokio::spawn(async move {
                    if late {
                        released.wait_for(|go| *go).await.expect("test alive");
                    }
                    let lag = Duration::from_secs(1);
                    let received =
                        commit(kind, &mut store, blocks, &committed, value_balance, lag, None);
                    let received = received.await;
                    (kind, store, committed, received)
                })
            })
            .collect();
        let mut indexed = nfs.indexed();
        let cancel = CancellationToken::new();
        let mut driver = tokio::spawn(nfs.run(cancel.clone()));
        let tree_state = |snapshot: &Indexed<DiskView>| {
            let durable = snapshot.durable().find(|(kind, _)| *kind == IndexKind::TreeState);
            durable.map(|(_, tip)| (snapshot.served().views().tree_state().is_some(), tip))
        };

        // (blocks added, settle every index: tree-state released) per move
        let moves: Vec<(&[Arc<Block>], bool)> = match run {
            0 => vec![(&a[1..=8], true)],
            _ => [a[10..].chunks(1).map(|added| (added, false)).collect(), vec![(&[][..], true)]]
                .concat(),
        };
        let (mut served, mut joined) = (Height::GENESIS, false);
        for (at, &(added, settle)) in moves.iter().enumerate() {
            let context = format!("run {run} move {at}");
            let (best, final_height) = add(added);
            release.send_replace(settle);
            let mut seen: Option<Arc<Indexed<DiskView>>> = None;
            let settled = async {
                loop {
                    let new = |latest: &Arc<_>| {
                        !seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, latest))
                    };
                    let latest = indexed.borrow_and_update().clone();
                    if let Some(latest) = latest.filter(new) {
                        verify(&latest, &blocks, &mined, &mut oracles, &context);
                        let tip = latest.served().tip().height;
                        assert!(tip >= served, "{context}: served {tip:?} back from {served:?}");
                        served = tip;
                        if let Some((present, durable)) = tree_state(&latest) {
                            let caught_up = durable.is_some_and(|durable| durable.height >= root);
                            assert!(
                                !present || caught_up,
                                "{context}: tree-state served at {durable:?}"
                            );
                            assert!(present || !joined, "{context}: tree-state gone after joining");
                            joined |= present;
                        }
                        seen = Some(latest);
                    }
                    let at_best = seen.as_ref().is_some_and(|seen| seen.served().tip() == best);
                    let durable = durable
                        .iter()
                        .all(|view| view.borrow().tip().map(|tip| tip.height) == final_height);
                    let complete = seen
                        .as_ref()
                        .and_then(|seen| tree_state(seen))
                        .is_none_or(|(present, _)| present);
                    if at_best && (!settle || durable && complete) {
                        return;
                    }
                    tokio::select! {
                        Ok(()) = indexed.changed() => {}
                        stopped = &mut driver => match stopped {
                            Err(join) => std::panic::resume_unwind(join.into_panic()),
                            Ok(result) => panic!("{context}: driver stopped: {result:?}"),
                        },
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
            };
            let day = Duration::from_secs(86_400);
            let settled = tokio::time::timeout(day, settled).await.is_ok();
            let tree_state = seen.as_deref().and_then(tree_state);
            if !settled {
                let durable: Vec<Option<BlockRef>> =
                    durable.iter().map(|view| view.borrow().tip()).collect();
                let served = seen.map(|seen| seen.served().tip());
                panic!(
                    "{context}: {best:?} not settled: served {served:?}, durable {durable:?}, \
                     tree-state (served, durable) {tree_state:?}"
                );
            }
            if run == 1 && !settle {
                let lagging = tree_state == Some((false, None));
                assert!(lagging, "{context}: {best:?} served, tree-state {tree_state:?}");
            }
        }

        cancel.cancel();
        driver.await.expect("driver task").expect("cancel = clean stop");
        let chain = verified_rx.borrow().clone().expect("verified");
        let final_height = chain.final_tip().map(|tip| tip.height).expect("final");
        for committer in committers {
            let (kind, store, committed, received) = committer.await.expect("committer task");
            let name = kind.name();
            let heights: Vec<Height> = received.iter().map(|(at, _)| at.height).collect();
            let expected: Vec<Height> = Height::GENESIS.up_to(final_height).collect();
            assert_eq!(heights, expected, "run {run}: N5 {name} each height once, through final");
            let folded: Vec<bool> = received.iter().map(|(_, folded)| *folded).collect();
            assert!(folded.is_sorted(), "run {run}: {name} unfolded after folded {folded:?}");
            indexes.push((kind, store, committed));
        }
    }
    stop_balancing.cancel();
}

/// Chain A 0..=5 (final 2), block-hash alone: `compact_block` before `value_balance` or an index
/// twice refused at subscribe; a committed X1 (A1's sibling) stops the run as `Diverged` naming
/// the index, before any step; a writer dropping its committed view stops it as `WriterGone`
#[tokio::test(start_paused = true)]
async fn the_driver_refuses_a_misordered_subscribe_a_foreign_durable_block_and_a_lost_writer() {
    let mut blocks = MockChain::regtest();
    let a5 = blocks.mine_empty(5);
    let a = blocks.blocks(a5);
    let x1 = blocks.fork(h(0)).mine_empty(1).tip().hash;
    let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
    let mut headers = blocks.header_chain(depth);
    insert(&mut headers, &a).expect("valid headers");
    headers.finalize(headers.finalizable().expect("5 - 3")).expect("in-memory store");
    let (_verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let params = ChainParams::of(&blocks, a5);
    let (queue, lookahead) = (NonZeroUsize::MAX, NonZeroUsize::MIN);
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let source = Arc::new(MockValidator::following(&blocks, a5));
    let (balancer, _never_driven) =
        TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);
    let nfs = || -> Nfs<_, DiskView> {
        Nfs::new(verified_rx.clone(), balancer.clone(), params, lookahead)
    };
    let engine = DiskEngine::new(SimFs::new());
    let schema = schema(IndexKind::BlockHash);
    let mut store = engine.open(Path::new("/foreign"), &schema).expect("fresh store");

    let committed = watch::channel(store.view()).0;
    let refused = [
        (
            "compact_block folds on value_balance's fees",
            fired(|| drop(nfs().subscribe(IndexKind::CompactBlock, committed.subscribe(), queue))),
        ),
        (
            "block_hash: twice",
            fired(|| {
                let mut nfs = nfs();
                let _first = nfs.subscribe(IndexKind::BlockHash, committed.subscribe(), queue);
                drop(nfs.subscribe(IndexKind::BlockHash, committed.subscribe(), queue));
            }),
        ),
    ];
    for (expected, message) in refused {
        let message = message.unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }

    for block in [&a[0], blocks.block(x1)] {
        let mut out = store.changes(block.at());
        let parent = BlockHashReader::new(store.staged());
        zaino_internal_block_hash_to_height::fold(&parent, block, &mut out);
        store.apply(out);
    }
    store.commit().expect("SimFs commit");
    committed.send_replace(store.view());
    let mut foreign = nfs();
    let mut stream = foreign.subscribe(IndexKind::BlockHash, committed.subscribe(), queue);
    let stopped = foreign.run(CancellationToken::new()).await.expect_err("X1 off the final chain");
    let a1 = a[1].header().hash;
    assert!(
        matches!(stopped, NfsError::Diverged { index: "block_hash", height, expected, got }
            if u32::from(height) == 1 && expected == x1 && got == a1),
        "{stopped}"
    );
    assert!(matches!(stream.next().await, Step::Shutdown), "no step before the stop");

    let fresh = engine.open(Path::new("/fresh"), &schema).expect("fresh store");
    let (committed, view) = watch::channel(fresh.view());
    let mut lost = nfs();
    let _stream = lost.subscribe(IndexKind::BlockHash, view, queue);
    let run = tokio::spawn(lost.run(CancellationToken::new()));
    drop(committed);
    let stopped = run.await.expect("driver task").expect_err("writer gone");
    assert!(matches!(stopped, NfsError::WriterGone("block_hash")), "{stopped}");
}
