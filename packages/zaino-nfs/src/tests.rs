//! [`Nfs`] end to end over the real final path: `FinalFollower` + every index writer, `SimFs`
//! stores, mock validators (`nfs.md`)
//!
//! - Oracle = each index's own fold from genesis along best, into fresh stores
//! - Every snapshot seen: tip on its best; `at` of every mined block (served, side, root): each
//!   index = the oracle at its view's tip, that tip = the block's (R12: a durable-only view ahead)

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockReader};
use zaino_index_transparent_address::{TransparentAddressIndexWriter, TransparentAddressReader};
use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateReader};
use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashReader};
use zaino_internal_value_balance::{ValueBalanceIndexWriter, ValueBalanceReader};
use zaino_persistence::{
    fs::SimFs, BlockChanges, DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema, Store,
    View, Width,
};
use zaino_primitives::testing::{p2pkh, MockChain};
use zaino_primitives::types::{Block, BlockHash, Height, OutPoint};
use zaino_source::testing::{Lie, MockValidator};
use zaino_sync::{FeeSink, FinalFollower, FollowError, SyncProgress};
use zaino_traffic::{Limits, Trusted};
use zcash_protocol::consensus::NetworkType;

use super::*;

const NETWORK: NetworkType = NetworkType::Regtest;
const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const ALL: [IndexKind; 5] = [
    IndexKind::ValueBalance,
    IndexKind::CompactBlock,
    IndexKind::BlockHash,
    IndexKind::TreeState,
    IndexKind::TransparentAddress,
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
    out: &mut BlockChanges,
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
        ALL.iter().map(|&kind| (kind, open(kind).expect("fresh store"))).collect();
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
        folded.push((kind, tables(&store.committed())));
    }
    folded
}

/// - N4: tip on its chain's best = the lowest view (a root snapshot: the lowest durable tip)
/// - G7: `at` of every mined block, served included: `Some` iff folded or the root; branch = the
///   block's path vs the chain's; every enabled index readable, through the block or (durable at
///   or past it) its durable tip alone (R12); either = the oracle there
/// - R12: `answers_through` = that durable tip on the best branch, the block on a side branch
fn verify(
    snapshot: &Indexed<DiskView>,
    blocks: &MockChain,
    mined: &[BlockHash],
    oracles: &mut HashMap<BlockHash, Vec<(IndexKind, Tables)>>,
    context: &str,
) {
    let (chain, tip) = (snapshot.chain(), snapshot.served().tip());
    assert!(chain.on_best(tip), "{context}: N4 tip {tip:?} off best");
    assert_eq!(snapshot.served().branch(), Branch::Best, "{context}: N4 served on the best");
    let durable: Vec<(IndexKind, Option<BlockRef>)> = snapshot.durable().collect();
    let views = snapshot.served().views();
    let lowest = durable.iter().filter_map(|(kind, _)| views.view(*kind)?.tip());
    if let Some(lowest) = lowest.map(|at| at.height).min() {
        assert_eq!(lowest, tip.height, "{context}: N4 the tip = the lowest view served");
    }

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
        let on_best = |h: usize| chain.on_best(BlockRef { hash: path[h], height: height(h) });
        let shared = (0..path.len()).take_while(|&h| on_best(h));
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
            let view = at.views().view(*kind);
            let view = view.unwrap_or_else(|| panic!("{context}: {name} unreadable at {block:?}"));
            assert_eq!(view.tip(), Some(through), "{context}: G7 {name} at {block:?}");
            let reach = match branch {
                Branch::Best => through.height,
                Branch::Side { .. } => block.height,
            };
            let answers = at.answers_through(*kind);
            assert_eq!(answers, reach, "{context}: R12 {name} answers through, at {block:?}");
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

/// One daemon's worth of pipeline over `engine`'s stores (infrastructure: no values under test)
struct Pipeline {
    indexed: Published<DiskView>,
    handles: Vec<(IndexKind, IndexHandle<DiskView>)>,
    progress: SyncProgress,
    cancel: CancellationToken,
    follower: JoinHandle<Result<(), FollowError>>,
    nfs: JoinHandle<Result<(), NfsError>>,
    writers: tokio::task::JoinSet<()>,
}

impl Pipeline {
    /// `kinds` in fold order (value-balance before compact-block), each writer as zainod wires it
    fn start(
        engine: &DiskEngine,
        kinds: &[IndexKind],
        chain: &watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: &TrafficBalancer<MockValidator>,
        params: ChainParams,
    ) -> Self {
        let lookahead = NonZeroUsize::new(4).expect("nonzero");
        let (queue, batch) = (NonZeroUsize::new(1 << 24).expect("nz"), NonZeroUsize::MIN);
        let mut follower = FinalFollower::new(chain.clone(), balancer.clone(), lookahead);
        let mut nfs = Nfs::new(chain.clone(), balancer.clone(), params, DEPTH, lookahead);
        let mut fee_sink = FeeSink::new("fees");
        let mut fees = kinds
            .contains(&IndexKind::CompactBlock)
            .then(|| fee_sink.subscribe(IndexKind::CompactBlock.name(), queue));
        let mut fee_sink = Some(fee_sink);
        let (mut writers, mut handles) = (tokio::task::JoinSet::new(), Vec::new());
        for &kind in kinds {
            let store = engine.open(Path::new(kind.name()), &schema(kind)).expect("store");
            let mut add = |handle: IndexHandle<DiskView>| {
                handles.push((kind, handle.clone()));
                let blocks = follower.subscribe(kind, handle.tip(), queue);
                nfs.add(kind, handle);
                blocks
            };
            match kind {
                IndexKind::ValueBalance => {
                    let writer = ValueBalanceIndexWriter::new(store, batch);
                    let blocks = add(writer.handle());
                    writers.spawn(writer.run(blocks, fee_sink.take().expect("one value-balance")));
                }
                IndexKind::CompactBlock => {
                    let writer = CompactBlockIndexWriter::new(store, batch);
                    let blocks = add(writer.handle());
                    writers.spawn(writer.run(blocks, fees.take().expect("one compact-block")));
                }
                IndexKind::BlockHash => {
                    let writer = BlockHashIndexWriter::new(store, batch);
                    let blocks = add(writer.handle());
                    writers.spawn(writer.run(blocks));
                }
                IndexKind::TreeState => {
                    let writer = TreeStateIndexWriter::new(store, batch);
                    let blocks = add(writer.handle());
                    writers.spawn(writer.run(blocks));
                }
                IndexKind::TransparentAddress => {
                    let writer = TransparentAddressIndexWriter::new(store, batch);
                    let blocks = add(writer.handle());
                    writers.spawn(writer.run(blocks));
                }
            }
        }
        if let Some(unused) = fee_sink {
            unused.shutdown();
        }
        let (indexed, progress, cancel) =
            (nfs.indexed(), follower.progress(), CancellationToken::new());
        let follower = tokio::spawn(follower.run(cancel.clone()));
        let nfs = tokio::spawn(nfs.run(cancel.clone()));
        Self { indexed, handles, progress, cancel, follower, nfs, writers }
    }

    fn durable(&self) -> Vec<Option<Height>> {
        self.handles.iter().map(|(_, handle)| handle.tip().map(|tip| tip.height)).collect()
    }

    /// Cancel → both clean `Ok`, every writer through `Shutdown`
    async fn stop(mut self) {
        self.cancel.cancel();
        self.follower.await.expect("follower task").expect("cancel = clean stop");
        self.nfs.await.expect("nfs task").expect("cancel = clean stop");
        while let Some(writer) = self.writers.join_next().await {
            writer.expect("writer through Shutdown");
        }
    }

    /// Every new publish `verify`'d (+ `each`) until every index durable at `final_height` and the
    /// served tip at `best` (a day of virtual time, else panic)
    async fn settle(
        &mut self,
        best: BlockRef,
        final_height: Option<Height>,
        mut each: impl FnMut(&Indexed<DiskView>),
        check: &mut impl FnMut(&Indexed<DiskView>, &str),
        context: &str,
    ) {
        let mut seen: Option<Arc<Indexed<DiskView>>> = None;
        let settled = async {
            loop {
                let latest = self.indexed.borrow_and_update().clone();
                let new = |latest: &Arc<_>| !seen.as_ref().is_some_and(|s| Arc::ptr_eq(s, latest));
                if let Some(latest) = latest.filter(new) {
                    check(&latest, context);
                    each(&latest);
                    seen = Some(latest);
                }
                let durable = self.durable().iter().all(|tip| *tip == final_height);
                if durable && seen.as_ref().is_some_and(|seen| seen.served().tip() == best) {
                    return;
                }
                assert!(!self.nfs.is_finished(), "{context}: NFS stopped");
                assert!(!self.follower.is_finished(), "{context}: follower stopped");
                tokio::select! {
                    Ok(()) = self.indexed.changed() => {}
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(86_400), settled).await.is_err() {
            let durable = self.durable();
            panic!("{context}: never settled: {best:?}, durable {durable:?} != {final_height:?}");
        }
    }
}

/// `count` blocks on `parent`, the first outweighing the best when `outweigh`: each a coinbase
/// paying 10 000 + a spend of its parent's coinbase (fee 1 000), one sapling output, one orchard
/// action; `tag` = a counter unique per mined block (note commitment + nullifier)
fn mine(
    blocks: &mut MockChain,
    tag: &mut u32,
    parent: BlockRef,
    count: u32,
    outweigh: bool,
) -> Vec<Arc<Block>> {
    let miner = p2pkh([0xad; 20]);
    let mut mined = Vec::new();
    let mut tip = parent;
    for at in 0..count {
        *tag += 1;
        let tag = *tag;
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
}

/// Block tree (mined up front), then two pipelines over the same stores, each move settled
/// (every index durable through final, served tip = best):
///
/// - Run 0: A 1..=12 (final 9: bulk, then tip), B 10..=13 off A9 (longer, heavier), C13 off B12
///   (same height), D12 off B11 (retreat), E 13..=16 (final 13), E 17..=20 (final 17)
/// - Run 1: restart from those tips, E 21..=24 (final 21)
/// - Bodies through the balancer: an honest member + a `Lie::Poisoned` liar (each lie reported,
///   re-asked; never folded: the oracle would differ)
/// - Final path: progress handed = the final tip after each move (none: nothing new since boot)
#[tokio::test(start_paused = true)]
async fn every_snapshot_answers_like_folding_from_genesis_through_reorgs_finality_and_a_restart() {
    let miner = p2pkh([0xad; 20]);
    let mut blocks =
        MockChain::regtest().varied_work().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    let hash = |block: &Arc<Block>| block.header().hash;
    let (genesis, mut tag) = (blocks.genesis(), 0);
    let trunk_a =
        [blocks.blocks(genesis), mine(&mut blocks, &mut tag, genesis, 12, false)].concat();
    let longer_b = mine(&mut blocks, &mut tag, trunk_a[9].at(), 4, true);
    let same_height_c13 = mine(&mut blocks, &mut tag, longer_b[2].at(), 1, true).remove(0);
    let retreat_d12 = mine(&mut blocks, &mut tag, longer_b[1].at(), 1, true).remove(0);
    let extension_e = mine(&mut blocks, &mut tag, retreat_d12.at(), 12, false);
    let blocks = blocks;
    let side = [same_height_c13.clone(), retreat_d12.clone()];
    let tree = [&trunk_a[..], &longer_b, &side, &extension_e];
    let mined: Vec<BlockHash> = tree.iter().flat_map(|run| run.iter().map(hash)).collect();
    type Added<'a> = (&'a [Arc<Block>], bool);
    // (headers added, finalize)
    let runs: [&[Added]; 2] = [
        &[
            (&trunk_a[1..], true),
            (&longer_b, false),
            (std::slice::from_ref(&same_height_c13), false),
            (std::slice::from_ref(&retreat_d12), false),
            (&extension_e[..4], true),
            (&extension_e[4..8], true),
        ],
        &[(&[], false), (&extension_e[8..], true)],
    ];

    let mut headers = blocks.header_chain(DEPTH);
    let members = [None, Some(Lie::Poisoned)].map(|lie| {
        let member = MockValidator::following(&blocks, genesis);
        member.lie(lie);
        Arc::new(member)
    });
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted =
        members.iter().map(|member| Trusted { source: Arc::clone(member), priority: 0, limits });
    let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let params = ChainParams::of(&blocks, genesis);
    let engine = DiskEngine::new(SimFs::new());
    let mut oracles = HashMap::new();
    let mut check = |snapshot: &Indexed<DiskView>, context: &str| {
        verify(snapshot, &blocks, &mined, &mut oracles, context)
    };

    for (run, moves) in runs.iter().enumerate() {
        let mut pipeline = Pipeline::start(&engine, &ALL, &verified_rx, &balancer, params);
        let booted_at = pipeline.durable().into_iter().min().flatten();
        for (at, &(added, finalize)) in moves.iter().enumerate() {
            let context = format!("run {run} move {at}");
            insert(&mut headers, added).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary);
            }
            if let Some(tip) = added.last() {
                members.iter().for_each(|member| member.follow(&blocks, tip.at()));
            }
            verified.send_replace(headers.verified().map(Arc::new));
            let best = headers.best().expect("verified").block;
            let final_height = headers.final_tip().map(|tip| tip.height);
            pipeline.settle(best, final_height, |_| {}, &mut check, &context).await;
            let sent_since_boot =
                final_height.filter(|final_height| Some(*final_height) > booted_at);
            let handed = pipeline.progress.handed();
            assert_eq!(handed, sent_since_boot, "{context}: final path handed through final");
        }
        pipeline.stop().await;
    }
    stop_balancing.cancel();
}

/// - Run 0: four indexes synced through A8 (final 5)
/// - Run 1: tree-state enabled on a fresh store; A 9..=16 one at a time (final = best − 3)
/// - Served tip = the lowest durable tip until the window holds it, never back; tree-state
///   readable in every snapshot; every snapshot = folding from genesis (`verify`)
/// - Settled: every index durable through final, served at best
#[tokio::test(start_paused = true)]
async fn an_index_enabled_late_holds_the_served_tip_back_until_it_catches_up() {
    let miner = p2pkh([0xad; 20]);
    let mut blocks = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    let genesis = blocks.genesis();
    let trunk = [blocks.blocks(genesis), mine(&mut blocks, &mut 0, genesis, 16, false)].concat();
    let blocks = blocks;
    let mined: Vec<BlockHash> = trunk.iter().map(|block| block.header().hash).collect();
    let mut headers = blocks.header_chain(DEPTH);
    let member = Arc::new(MockValidator::following(&blocks, genesis));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = vec![Trusted { source: Arc::clone(&member), priority: 0, limits }];
    let (balancer, balancing) = TrafficBalancer::new(trusted, None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let params = ChainParams::of(&blocks, trunk[16].at());
    let engine = DiskEngine::new(SimFs::new());
    let mut oracles = HashMap::new();
    let mut check = |snapshot: &Indexed<DiskView>, context: &str| {
        verify(snapshot, &blocks, &mined, &mut oracles, context)
    };
    let four = [
        IndexKind::ValueBalance,
        IndexKind::CompactBlock,
        IndexKind::BlockHash,
        IndexKind::TransparentAddress,
    ];
    let mut add = |added: &[Arc<Block>]| {
        insert(&mut headers, added).expect("valid headers");
        if let Some(boundary) = headers.finalizable() {
            headers.finalize(boundary);
        }
        if let Some(tip) = added.last() {
            member.follow(&blocks, tip.at());
        }
        verified.send_replace(headers.verified().map(Arc::new));
        (headers.best().expect("verified").block, headers.final_tip().map(|tip| tip.height))
    };

    let mut pipeline = Pipeline::start(&engine, &four, &verified_rx, &balancer, params);
    let (best, final_height) = add(&trunk[1..=8]);
    pipeline.settle(best, final_height, |_| {}, &mut check, "run 0").await;
    pipeline.stop().await;

    let five = [&four[..3], &[IndexKind::TreeState], &four[3..]].concat();
    let mut pipeline = Pipeline::start(&engine, &five, &verified_rx, &balancer, params);
    let mut served = Height::GENESIS;
    for at in 9..=16 {
        let context = format!("run 1 A{at}");
        let (best, final_height) = add(&trunk[at..=at]);
        let each = |snapshot: &Indexed<DiskView>| {
            let tip = snapshot.served().tip().height;
            assert!(tip >= served, "{context}: served {tip:?} back from {served:?}");
            served = tip;
            let readable = snapshot.served().views().tree_state().is_some();
            assert!(readable, "{context}: tree-state readable at {tip:?}");
        };
        pipeline.settle(best, final_height, each, &mut check, &context).await;
    }
    pipeline.stop().await;
    stop_balancing.cancel();
}

/// A writer gone (its stream ended under it): the NFS stops as `IndexGone` naming it
#[tokio::test(start_paused = true)]
async fn the_nfs_stops_when_an_index_writer_is_gone() {
    let mut blocks = MockChain::regtest();
    let a5 = blocks.mine_empty(5);
    let mut headers = blocks.header_chain(DEPTH);
    insert(&mut headers, &blocks.blocks(a5)).expect("valid headers");
    let (_verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let source = Arc::new(MockValidator::following(&blocks, a5));
    let (balancer, _never_driven) =
        TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);
    let engine = DiskEngine::new(SimFs::new());
    let store = engine.open(Path::new("block_hash"), &schema(IndexKind::BlockHash)).expect("store");
    let writer = BlockHashIndexWriter::new(store, NonZeroUsize::MIN);
    let lookahead = NonZeroUsize::MIN;
    let params = ChainParams::of(&blocks, a5);
    let mut nfs: Nfs<_, DiskView> = Nfs::new(verified_rx, balancer, params, DEPTH, lookahead);
    nfs.add(IndexKind::BlockHash, writer.handle());
    let mut sink = zaino_sync::IndexerDataSink::new("final");
    let blocks = sink.subscribe(IndexKind::BlockHash.name(), NonZeroUsize::MAX);
    let run = tokio::spawn(nfs.run(CancellationToken::new()));
    sink.shutdown();
    writer.run(blocks).await;
    let stopped = run.await.expect("nfs task").expect_err("writer gone");
    assert!(matches!(stopped, NfsError::IndexGone("block_hash")), "{stopped}");
}
