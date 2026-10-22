//! [`Nfs`] end to end over the real finalized path: `FinalizedFollower` + every index writer,
//! `SimFs` stores, mock validators (`usage.md`)
//!
//! - Oracle = each index's own fold from genesis along best, into fresh stores
//! - Every publication seen: tip on its best
//! - `at` of every mined block (served, side, root): each index = the oracle at its view's tip,
//!   that tip = the block's (or a durable-only view ahead)

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::testing::insert;
use zaino_header_chain::HeaderChain;
use zaino_persistence::{
    fs::SimFs, DiskEngine, DiskStore, DiskView, MapRead, PersistenceEngine, SequenceRead, Store,
    View,
};
use zaino_primitives::types::Block;
use zaino_source::testing::{p2pkh, Lie, MockChain};
use zaino_source::ZcashLegacyRpcClient;
use zaino_sync::{FinalizedFollower, FollowError, SyncProgress};
use zaino_traffic::{Limits, TrustedValidator};
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use super::*;
use crate::{fold, schema, writer, FeeWiring};

pub(crate) const NETWORK: NetworkType = NetworkType::Regtest;
const DEPTH: FinalityDepth = FinalityDepth::new(NonZeroU32::new(3).expect("nonzero"));

/// Every record and row, table by table (engine-agnostic equality)
type Tables = Vec<Vec<Vec<u8>>>;

/// Every record and row of `view`, table by table
fn tables(view: &(impl SequenceRead + MapRead)) -> Tables {
    let schema = *view.schema();
    let sequences = schema.sequences().iter().map(|&table| {
        let table = view.sequence(table);
        let records = table.records(0..table.len());
        records.iter().map(|record| record.to_vec()).collect()
    });
    let maps = schema.maps().iter().map(|&table| {
        let past = vec![0xff; table.key.get() as usize + 1];
        let rows = view.map(table).range(&[], &past, usize::MAX).expect("under the limit");
        rows.iter().map(|(key, value)| [&key[..], &value[..]].concat()).collect()
    });
    sequences.chain(maps).collect()
}

/// Every index folded from genesis through `path` by its own fold, into fresh stores
fn oracle(path: &[Arc<Block>]) -> Vec<(IndexKind, Tables)> {
    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let open = |kind: IndexKind| {
        engine.open(Path::new(kind.name()), &schema(kind, NETWORK), NonZeroUsize::MAX)
    };
    let mut stores: Vec<(IndexKind, DiskStore)> =
        IndexKind::ALL.iter().map(|&kind| (kind, open(kind).expect("fresh store"))).collect();
    for block in path {
        let mut fees = None;
        for (kind, store) in &mut stores {
            let mut out = store.changes(block.block_ref());
            let folded = fold(*kind, store.applied(), block, fees.as_ref(), &mut out);
            fees = folded.expect("canonical block").or(fees);
            store.apply(out);
        }
    }
    let mut folded = Vec::new();
    for (kind, mut store) in stores {
        store.commit().expect("SimFs commit");
        folded.push((kind, tables(&store.durable())));
    }
    folded
}

/// - N4: tip on its chain's best = the lowest view (a root publication: the lowest durable tip)
/// - N7: `at` of every mined block, served included: `Some` iff folded or the root
/// - N7 branch = the block's path vs the chain's
/// - N7 every enabled index readable, through the block or (durable at or past it) its durable tip
///   alone, either = the oracle there
/// - `answers_through` = that durable tip on the best branch, the block on a side branch
fn verify(
    indexed: &NfsPublication<DiskView>,
    blocks: &MockChain,
    mined: &[BlockHash],
    oracles: &mut HashMap<BlockHash, Vec<(IndexKind, Tables)>>,
    context: &str,
) {
    let (chain, tip) = (indexed.chain(), indexed.served().tip());
    assert!(chain.on_best(tip), "{context}: N4 tip {tip:?} off best");
    let durable: Vec<(IndexKind, Option<BlockRef>)> = indexed.durable().collect();
    let views = indexed.served().views();
    let lowest = durable.iter().filter_map(|(kind, _)| views.view(*kind)?.tip());
    if let Some(lowest) = lowest.map(|at| at.height).min() {
        assert_eq!(lowest, tip.height, "{context}: N4 the tip = the lowest view served");
    }
    let above_root = indexed.root().map_or(0, |root| u32::from(root.height) as usize + 1);
    let path = blocks.blocks(tip);
    let expected: Vec<BlockHash> =
        path[above_root..].iter().rev().map(|b| b.header().hash).collect();
    let above_root: Vec<BlockHash> =
        indexed.served_above_durable().map(|b| b.header().hash).collect();
    let message = "served_above_durable = served's own path above the root";
    assert_eq!(above_root, expected, "{context}: {message}");

    for hash in mined {
        let Some(at) = indexed.at(hash) else {
            let held =
                indexed.folded(hash) || indexed.root().is_some_and(|root| root.hash == *hash);
            assert!(!held, "{context}: N7 at({hash:?}) = None for a folded block or the root");
            continue;
        };
        let block = at.tip();
        let path = blocks.blocks(blocks.block(*hash).block_ref());
        let on_best = path.iter().all(|block| chain.on_best(block.block_ref()));
        assert_eq!(block.hash, *hash, "{context}: N7 at({hash:?})");
        for (kind, durable) in &durable {
            let name = kind.name();
            let through = durable.filter(|durable| durable.height >= block.height).unwrap_or(block);
            let view = at.views().view(*kind);
            let view = view.unwrap_or_else(|| panic!("{context}: {name} unreadable at {block:?}"));
            assert_eq!(view.tip(), Some(through), "{context}: N7 {name} at {block:?}");
            let reach = if on_best { through.height } else { block.height };
            let answers = at.answers_through(*kind);
            assert_eq!(answers, reach, "{context}: {name} answers_through, at {block:?}");
            let expected =
                oracles.entry(through.hash).or_insert_with(|| oracle(&blocks.blocks(through)));
            let (_, expected) =
                expected.iter().find(|(each, _)| each == kind).expect("every index");
            let got = tables(&view);
            assert!(got == *expected, "{context}: N6 {name} at {through:?} != folded from genesis");
        }
    }
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
    /// `kinds` in fold order (utxo-set before compact-block), each writer as zainod wires it
    fn start(
        engine: &DiskEngine,
        kinds: &[IndexKind],
        chain: &watch::Receiver<Option<Arc<HeaderChainSnapshot>>>,
        balancer: &TrafficBalancer<ZcashLegacyRpcClient>,
        params: ChainParams,
    ) -> Self {
        let lookahead = NonZeroUsize::new(4).expect("nonzero");
        let queue = NonZeroUsize::new(1 << 24).expect("nz");
        let mut follower = FinalizedFollower::new(chain.clone(), balancer.clone(), lookahead);
        let mut nfs = Nfs::new(chain.clone(), balancer.clone(), params, DEPTH, lookahead);
        let mut fees = FeeWiring::new(kinds.contains(&IndexKind::CompactBlock).then_some(queue));
        let (mut writers, mut handles) = (tokio::task::JoinSet::new(), Vec::new());
        for &kind in kinds {
            let store =
                engine.open(Path::new(kind.name()), &schema(kind, NETWORK), NonZeroUsize::MIN);
            let (handle, run) = writer(kind, store.expect("store"), &mut fees);
            handles.push((kind, handle.clone()));
            let blocks = follower.subscribe(kind, handle.durable_view().tip(), queue);
            nfs.add(kind, handle);
            writers.spawn(run(blocks));
        }
        let (indexed, progress, cancel) =
            (nfs.indexed(), follower.progress(), CancellationToken::new());
        let follower = tokio::spawn(follower.run(cancel.clone()));
        let nfs = tokio::spawn(nfs.run(cancel.clone()));
        Self { indexed, handles, progress, cancel, follower, nfs, writers }
    }

    fn durable(&self) -> Vec<Option<BlockHeight>> {
        self.handles.iter().map(|(_, handle)| handle.durable_height()).collect()
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

    /// Every new publish `verify`'d (+ `each`) until every index durable at `finalized_height` and
    /// the served tip at `best` (a day of virtual time, else panic)
    async fn settle(
        &mut self,
        best: BlockRef,
        finalized_height: Option<BlockHeight>,
        mut each: impl FnMut(&NfsPublication<DiskView>),
        check: &mut impl FnMut(&NfsPublication<DiskView>, &str),
        context: &str,
    ) {
        let mut seen: Option<Arc<NfsPublication<DiskView>>> = None;
        let settled = async {
            loop {
                let latest = self.indexed.borrow_and_update().clone();
                let new = |latest: &Arc<_>| !seen.as_ref().is_some_and(|s| Arc::ptr_eq(s, latest));
                if let Some(latest) = latest.filter(new) {
                    check(&latest, context);
                    each(&latest);
                    seen = Some(latest);
                }
                let durable = self.durable().iter().all(|tip| *tip == finalized_height);
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
            panic!(
                "{context}: never settled: {best:?}, durable {durable:?} != {finalized_height:?}"
            );
        }
    }
}

/// `count` blocks on `parent`, the first outweighing the best when `outweigh`
///
/// - each: coinbase paying 10 000 + a spend of its parent's coinbase (fee 1 000), one sapling
///   output, one orchard action
/// - `tag` = a counter unique per mined block (note commitment + nullifier)
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
        let funding = blocks.output(tip, 0, 0);
        let branch = blocks.branch(tip);
        let branch = if outweigh && at == 0 { branch.outweigh() } else { branch };
        tip = branch
            .mine(|b| {
                b.coinbase(|c| c.pay(&miner, 10_000)).tx(|t| {
                    t.spend(funding)
                        .pay(&miner, 9_000)
                        .fee(1_000)
                        .sapling_output(tag)
                        .orchard_action(tag, tag)
                })
            })
            .tip();
        mined.push(Arc::clone(blocks.block(tip.hash)));
    }
    mined
}

/// Two pipelines over the same stores, each move settled (durable through finalized, served = best)
///
/// - run 0: A1..=12, B10..=13 off A9, C13 off B12, D12 off B11, E13..=20
/// - run 1: restart, E21..=24
/// - bodies from an honest member + a `Lie::Poisoned` liar (each lie reported, never folded)
#[tokio::test(start_paused = true)]
async fn every_publication_answers_like_folding_from_genesis_through_reorgs_finality_and_a_restart()
{
    let miner = p2pkh([0xad; 20]);
    let mut blocks =
        MockChain::regtest().varied_work().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    let hash = |block: &Arc<Block>| block.header().hash;
    let (genesis, mut tag) = (blocks.genesis(), 0);
    let trunk_a =
        [blocks.blocks(genesis), mine(&mut blocks, &mut tag, genesis, 12, false)].concat();
    let longer_b = mine(&mut blocks, &mut tag, trunk_a[9].block_ref(), 4, true);
    let same_height_c13 = mine(&mut blocks, &mut tag, longer_b[2].block_ref(), 1, true).remove(0);
    let retreat_d12 = mine(&mut blocks, &mut tag, longer_b[1].block_ref(), 1, true).remove(0);
    let extension_e = mine(&mut blocks, &mut tag, retreat_d12.block_ref(), 12, false);
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

    let mut headers = HeaderChain::anchored(&blocks, DEPTH);
    let members = [None, Some(Lie::Poisoned)].map(|lie| {
        let member = blocks.validator(genesis);
        member.lie(lie);
        member
    });
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = members.iter().enumerate().map(|(index, member)| TrustedValidator {
        address: format!("member{index}"),
        source: Arc::new(member.client()),
        priority: 0,
        limits,
    });
    let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.snapshot().map(Arc::new));
    let params = ChainParams::of(&blocks);
    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let mut oracles = HashMap::new();
    let mut check = |indexed: &NfsPublication<DiskView>, context: &str| {
        verify(indexed, &blocks, &mined, &mut oracles, context)
    };

    for (run, moves) in runs.iter().enumerate() {
        let mut pipeline =
            Pipeline::start(&engine, &IndexKind::ALL, &verified_rx, &balancer, params);
        let booted_at = pipeline.durable().into_iter().min().flatten();
        for (at, &(added, finalize)) in moves.iter().enumerate() {
            let context = format!("run {run} move {at}");
            insert(&mut headers, added).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary);
            }
            if let Some(tip) = added.last() {
                members.iter().for_each(|member| member.follow(&blocks, tip.block_ref()));
            }
            verified.send_replace(headers.snapshot().map(Arc::new));
            let best = headers.best().expect("verified").block;
            let finalized_height = headers.finalized_tip().map(|tip| tip.height);
            pipeline.settle(best, finalized_height, |_| {}, &mut check, &context).await;
            let sent_since_boot =
                finalized_height.filter(|finalized_height| Some(*finalized_height) > booted_at);
            let handed = pipeline.progress.handed();
            assert_eq!(
                handed, sent_since_boot,
                "{context}: finalized path handed through finalized"
            );
        }
        pipeline.stop().await;
    }
    stop_balancing.cancel();
}

/// - Run 0: four indexes synced through A8 (finalized 5)
/// - Run 1: tree-state enabled on a fresh store, A 9..=16 one at a time (finalized = best − 3)
/// - Served tip = the lowest durable tip until the window holds it, never back
/// - Every publication: tree-state readable, = folding from genesis (`verify`)
/// - Settled: every index durable through finalized, served at best
#[tokio::test(start_paused = true)]
async fn an_index_enabled_late_holds_the_served_tip_back_until_it_catches_up() {
    let miner = p2pkh([0xad; 20]);
    let mut blocks = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&miner, 10_000)));
    let genesis = blocks.genesis();
    let trunk = [blocks.blocks(genesis), mine(&mut blocks, &mut 0, genesis, 16, false)].concat();
    let blocks = blocks;
    let mined: Vec<BlockHash> = trunk.iter().map(|block| block.header().hash).collect();
    let mut headers = HeaderChain::anchored(&blocks, DEPTH);
    let member = blocks.validator(genesis);
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (address, source) = ("member".to_owned(), Arc::new(member.client()));
    let trusted = vec![TrustedValidator { address, source, priority: 0, limits }];
    let (balancer, balancing) = TrafficBalancer::new(trusted, None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.snapshot().map(Arc::new));
    let params = ChainParams::of(&blocks);
    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let mut oracles = HashMap::new();
    let mut check = |indexed: &NfsPublication<DiskView>, context: &str| {
        verify(indexed, &blocks, &mined, &mut oracles, context)
    };
    let four = [
        IndexKind::UtxoSet,
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
            member.follow(&blocks, tip.block_ref());
        }
        verified.send_replace(headers.snapshot().map(Arc::new));
        (headers.best().expect("verified").block, headers.finalized_tip().map(|tip| tip.height))
    };

    let mut pipeline = Pipeline::start(&engine, &four, &verified_rx, &balancer, params);
    let (best, finalized_height) = add(&trunk[1..=8]);
    pipeline.settle(best, finalized_height, |_| {}, &mut check, "run 0").await;
    pipeline.stop().await;

    let five = [&four[..3], &[IndexKind::TreeState], &four[3..]].concat();
    let mut pipeline = Pipeline::start(&engine, &five, &verified_rx, &balancer, params);
    let mut served = H0;
    for at in 9..=16 {
        let context = format!("run 1 A{at}");
        let (best, finalized_height) = add(&trunk[at..=at]);
        let each = |indexed: &NfsPublication<DiskView>| {
            let tip = indexed.served().tip().height;
            assert!(tip >= served, "{context}: served {tip:?} back from {served:?}");
            served = tip;
            let readable = indexed.served().views().tree_state().is_some();
            assert!(readable, "{context}: tree-state readable at {tip:?}");
        };
        pipeline.settle(best, finalized_height, each, &mut check, &context).await;
    }
    pipeline.stop().await;
    stop_balancing.cancel();
}

/// A writer gone (its stream ended under it): the NFS stops as `IndexGone` naming it
#[tokio::test(start_paused = true)]
async fn the_nfs_stops_when_an_index_writer_is_gone() {
    let mut blocks = MockChain::regtest();
    let a5 = blocks.mine_empty(5);
    let mut headers = HeaderChain::anchored(&blocks, DEPTH);
    insert(&mut headers, &blocks.blocks(a5)).expect("valid headers");
    let (_verified, verified_rx) = watch::channel(headers.snapshot().map(Arc::new));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let source = Arc::new(blocks.validator(a5).client());
    let trusted = TrustedValidator { address: "unpolled".to_owned(), source, priority: 0, limits };
    let (balancer, _never_driven) = TrafficBalancer::new(vec![trusted], None);
    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let store = engine.open(
        Path::new("block_hash"),
        &schema(IndexKind::BlockHash, NETWORK),
        NonZeroUsize::MIN,
    );
    let fees = &mut FeeWiring::new(None);
    let (handle, write) = writer(IndexKind::BlockHash, store.expect("store"), fees);
    let lookahead = NonZeroUsize::MIN;
    let params = ChainParams::of(&blocks);
    let mut nfs: Nfs<_, DiskView> = Nfs::new(verified_rx, balancer, params, DEPTH, lookahead);
    nfs.add(IndexKind::BlockHash, handle);
    let mut sink = zaino_sync::FinalizedStream::new();
    let blocks = sink.subscribe(IndexKind::BlockHash.name(), NonZeroUsize::MAX);
    let run = tokio::spawn(nfs.run(CancellationToken::new()));
    sink.shutdown();
    write(blocks).await;
    let stopped = run.await.expect("nfs task").expect_err("writer gone");
    assert!(matches!(stopped, NfsError::IndexGone("block_hash")), "{stopped}");
}
