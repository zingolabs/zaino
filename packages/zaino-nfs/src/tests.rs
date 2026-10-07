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
use zaino_header_chain::HeaderChain;
use zaino_index_compact_block::CompactBlockReader;
use zaino_index_transparent_address::TransparentAddressReader;
use zaino_index_tree_state::{PoolActivations, TreeStateReader};
use zaino_internal_block_hash_to_height::BlockHashReader;
use zaino_internal_value_balance::ValueBalanceReader;
use zaino_persistence::{
    fs::SimFs, Changes, DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema, Store, View,
    Width,
};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{
    Block, BlockHash, CompactCiphertext, OrchardAction, OrchardData, OutPoint, SaplingData,
    SaplingOutput, Script, Transaction, TransactionId, TransparentData, TransparentOutput,
    Zatoshis,
};
use zaino_source::mock::MockChain;
use zaino_source::{
    BlockLinks, GetBlockByHashError, GetTransactionError, MempoolListed, NonDomainError,
    PollReading, QueryError, RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
};
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

/// `MockChain`, its block bodies with the coinbase twice when `lying` (merkle root moves)
#[derive(Default)]
struct Member {
    chain: MockChain,
    lying: bool,
}

impl ChainDataSource for Member {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        let block = self.chain.get_block_by_hash(hash).await?;
        let txs = block.transactions();
        Ok(match self.lying {
            true => Block::new(block.header().clone(), [txs, &txs[..1]].concat()),
            false => block,
        })
    }

    async fn get_block_links(&self, heights: &[Height]) -> Result<BlockLinks, NonDomainError> {
        self.chain.get_block_links(heights).await
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
    ) -> Result<PollReading, NonDomainError> {
        self.chain.get_poll_reading(metadata, holds).await
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        self.chain.get_raw_mempool_transactions(listed).await
    }

    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.chain.get_transaction(txid).await
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        self.chain.send_raw_transaction(transaction).await
    }
}

/// Coinbase paying 10 000, + a spend of `funding`'s output 0 (fee 1 000) carrying one sapling
/// output and one orchard action (none: genesis, or a funding coinbase paying nothing)
fn transactions(funding: Option<&Transaction>, tag: u32) -> Vec<Transaction> {
    let id = |kind: u8| {
        let mut id = [kind; 32];
        id[..4].copy_from_slice(&tag.to_le_bytes());
        id
    };
    let pays = |value: u64| TransparentOutput {
        value: Zatoshis::new(value).expect("in supply"),
        script: Script::new([&[0x76, 0xa9, 0x14][..], &id(0xad)[..20], &[0x88, 0xac]].concat()),
    };
    let commitment = || {
        let mut leaf = [0u8; 32];
        leaf[..4].copy_from_slice(&tag.to_le_bytes());
        leaf
    };
    let tx = |txid: [u8; 32], transparent: TransparentData| Transaction {
        txid: TransactionId::from(txid),
        transparent,
        sprout: Default::default(),
        sapling: Default::default(),
        orchard: Default::default(),
        ironwood: Default::default(),
    };
    let coinbase = TransparentData { coinbase: true, inputs: vec![], outputs: vec![pays(10_000)] };
    let mut txs = vec![tx(id(0xc0), coinbase)];
    let funded = funding.and_then(|funding| Some((funding, funding.transparent.outputs.first()?)));
    if let Some((funding, output)) = funded {
        let inputs = vec![OutPoint { txid: funding.txid, vout: 0 }];
        let outputs = vec![pays(output.value.as_u64() - 1_000)];
        let mut spend = tx(id(0x5e), TransparentData { coinbase: false, inputs, outputs });
        spend.sapling = SaplingData {
            outputs: vec![SaplingOutput {
                cmu: commitment().into(),
                ephemeral_key: [0x02; 32].into(),
                enc_ciphertext: CompactCiphertext::from([0x03; CompactCiphertext::LENGTH]),
            }],
            ..Default::default()
        };
        spend.orchard = OrchardData {
            actions: vec![OrchardAction {
                nullifier: id(0x0f).into(),
                cmx: commitment().into(),
                ephemeral_key: [0x06; 32].into(),
                enc_ciphertext: CompactCiphertext::from([0x07; CompactCiphertext::LENGTH]),
            }],
            ..Default::default()
        };
        txs.push(spend);
    }
    txs
}

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
fn oracle(path: &[Block]) -> Vec<(IndexKind, Tables)> {
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

/// Test-only writer: each `Final` applied (folded, else its own fold) and committed after `lag`
///
/// - A height already held skipped (restart: an index ahead of the root)
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
        let changes = match &data.folds {
            Some(folds) => folds.get(kind).expect("folded for every enabled index").clone(),
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

/// - N4: tip on its chain's best = the lowest index view (a root snapshot: its lowest index)
/// - G7: `at` of every mined block, served included: `Some` iff folded or the root; branch = the
///   block's path vs the chain's; each index reads through the block, or (durable at or past it)
///   its durable tip alone (R12); either = the oracle there
fn verify(
    snapshot: &Snapshot<DiskView>,
    blocks: &Chain,
    mined: &[BlockHash],
    oracles: &mut HashMap<BlockHash, Vec<(IndexKind, Tables)>>,
    context: &str,
) {
    let (chain, tip) = (snapshot.chain(), snapshot.tip());
    assert_eq!(chain.hash_at(tip.height), Some(tip.hash), "{context}: N4 tip {tip:?} off best");
    assert_eq!(snapshot.served().branch(), Branch::Best, "{context}: N4 served on the best");
    let durable: Vec<(IndexKind, Option<BlockRef>)> = snapshot.durable().collect();
    let root = durable.iter().filter_map(|(_, tip)| *tip).min_by_key(|tip| tip.height);
    let lowest = INDEXES.iter().filter_map(|&(kind, ..)| snapshot.views().view(kind)?.tip());
    let lowest = lowest.map(|at| at.height).min();
    assert_eq!(lowest, Some(tip.height), "{context}: N4 the tip = the lowest index view");

    for hash in mined {
        let Some(at) = snapshot.at(hash) else {
            let held = snapshot.folded(hash) || root.is_some_and(|root| root.hash == *hash);
            assert!(!held, "{context}: G7 at({hash:?}) = None for a folded block or the root");
            continue;
        };
        let block = at.tip();
        let path: Vec<BlockHash> = blocks.path(*hash).iter().map(|b| b.header().hash).collect();
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
            let view = at.views().view(*kind).expect("every index enabled");
            assert_eq!(view.tip(), Some(through), "{context}: G7 {name} at {block:?}");
            let expected =
                oracles.entry(through.hash).or_insert_with(|| oracle(&blocks.path(through.hash)));
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
/// - Bodies through the balancer: an honest member + a liar (each lie reported, re-asked; never
///   folded: the oracle would differ)
#[tokio::test(start_paused = true)]
async fn every_snapshot_answers_like_folding_from_genesis_through_reorgs_finality_and_a_restart() {
    let mut blocks = Chain::with_genesis(transactions(None, 0));
    let genesis = blocks.genesis().hash;
    let mut tag = 0;
    let mut mine = |blocks: &mut Chain, parent: BlockHash, count: u32| -> Vec<Block> {
        let mut tip = parent;
        for _ in 0..count {
            tag += 1;
            let funding = blocks.block(tip).transactions().first();
            tip = blocks.mine_with(tip, transactions(funding, tag)).hash;
        }
        let path = blocks.path(tip);
        path[path.len() - count as usize..].to_vec()
    };
    let hash = |block: &Block| block.header().hash;
    let a = [blocks.path(genesis), mine(&mut blocks, genesis, 12)].concat();
    let b10 = blocks.mine_heavier(hash(&a[9]), &a[10..].iter().map(hash).collect::<Vec<_>>());
    let b10 = blocks.block(b10.expect("work in range").hash).clone();
    let b = [vec![b10.clone()], mine(&mut blocks, hash(&b10), 3)].concat();
    let c13 = blocks.mine_heavier(hash(&b[2]), &[hash(&b[3])]).expect("work in range").hash;
    let d12 = blocks.mine_heavier(hash(&b[1]), &[hash(&b[2]), c13]).expect("work in range").hash;
    let e = mine(&mut blocks, d12, 12);
    let (c13, d12) = (blocks.block(c13).clone(), blocks.block(d12).clone());
    let blocks = blocks;
    let tree = [&a[..], &b, &[c13.clone(), d12.clone()], &e];
    let mined: Vec<BlockHash> = tree.iter().flat_map(|run| run.iter().map(hash)).collect();
    // (headers added, finalize)
    let runs: [&[(&[Block], bool)]; 2] = [
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
    let mut headers = HeaderChain::regtest_in_memory(genesis, depth);
    headers.insert_blocks(&a[..1]).expect("genesis");
    let members = [false, true]
        .map(|lying| Arc::new(Member { chain: MockChain::serving([a[0].clone()]), lying }));
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted =
        members.iter().map(|member| Trusted { source: Arc::clone(member), priority: 0, limits });
    let (balancer, balancing) = TrafficBalancer::new(trusted.collect(), None);
    let stop_balancing = CancellationToken::new();
    tokio::spawn(balancing.run(stop_balancing.clone()));
    let (verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let activations = PoolActivations {
        sapling: Height::GENESIS,
        orchard: Some(Height::GENESIS),
        ironwood: None,
    };
    let params = ChainParams { network: NETWORK, activations };
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
        let mut nfs = Nfs::new(verified_rx.clone(), balancer.clone(), params, lookahead, depth);
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
        let mut handle = nfs.handle();
        let cancel = CancellationToken::new();
        let mut driver = tokio::spawn(nfs.run(cancel.clone()));

        for (at, &(added, finalize)) in moves.iter().enumerate() {
            let context = format!("run {run} move {at}");
            headers.insert_blocks(added).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary).expect("in-memory store");
            }
            for member in &members {
                member.chain.extend_best(added.to_vec());
            }
            verified.send_replace(headers.verified().map(Arc::new));
            let best = headers.best().expect("verified").block;
            let final_height = headers.final_tip().map(|tip| tip.height);
            let durable_heights = || -> Vec<Option<Height>> {
                durable.iter().map(|view| view.borrow().tip().map(|tip| tip.height)).collect()
            };
            let settled_heights: Vec<Option<Height>> =
                crashes.iter().map(|crash| final_height.min(crash.or(final_height))).collect();
            let mut seen: Option<Arc<Snapshot<DiskView>>> = None;
            let settled = async {
                loop {
                    let new = |latest: &Arc<_>| {
                        !seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, latest))
                    };
                    if let Some(latest) = handle.snapshot().filter(new) {
                        verify(&latest, &blocks, &mined, &mut oracles, &context);
                        seen = Some(latest);
                    }
                    let durable = durable_heights() == settled_heights;
                    if durable && seen.as_ref().is_some_and(|seen| seen.tip() == best) {
                        return;
                    }
                    tokio::select! {
                        Ok(()) = handle.changed() => {}
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

/// Chain A 0..=5 (final 2), block-hash alone: `compact_block` before `value_balance` or an index
/// twice refused at subscribe; a committed X1 (A1's sibling) stops the run as `Diverged` naming
/// the index, before any step; a writer dropping its committed view stops it as `WriterGone`
#[tokio::test(start_paused = true)]
async fn the_driver_refuses_a_misordered_subscribe_a_foreign_durable_block_and_a_lost_writer() {
    let mut blocks = Chain::new();
    let genesis = blocks.genesis().hash;
    let a5 = blocks.extend(genesis, 5).hash;
    let a = blocks.path(a5);
    let x1 = blocks.mine(genesis).hash;
    let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
    let mut headers = HeaderChain::regtest_in_memory(genesis, depth);
    headers.insert_blocks(&a).expect("valid headers");
    headers.finalize(headers.finalizable().expect("5 - 3")).expect("in-memory store");
    let (_verified, verified_rx) = watch::channel(headers.verified().map(Arc::new));
    let activations = PoolActivations { sapling: Height::GENESIS, orchard: None, ironwood: None };
    let params = ChainParams { network: NETWORK, activations };
    let (queue, lookahead) = (NonZeroUsize::MAX, NonZeroUsize::MIN);
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let source = Arc::new(MockChain::serving(a.clone()));
    let (balancer, _never_driven) =
        TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);
    let nfs = || -> Nfs<_, DiskView> {
        Nfs::new(verified_rx.clone(), balancer.clone(), params, lookahead, depth)
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
