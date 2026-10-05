//! compact_block index: raw blocks → framed records, plus the commitment-tree sizes they carry,
//! kept by its own loop
//!
//! - only place tree sizes are computed ([`Block`] carries none; `z_gettreestate` = one round trip
//!   per block, unaffordable in a full sync)
//! - derived: size at `h` = size at `h - 1` + what `h` commits → block order non-negotiable (a gap
//!   silently mis-sizes every later block; `apply` asserts first)
//! - resume seeds the carry from the manifest (sizes committed with the tip)
//! - fees: one [`BlockFees`] step off value-balance's sink per block step, awaited after it

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use zaino_primitives::types::{Block, BlockFees, BlockRef, Height, TreeSizes};
use zaino_sync::{Offloaded, Published, Step, Subscription, Weight};

use crate::{encode_compact_block, CompactBlockStore, NonFinalizedState, ReadView, Snapshot, HASH};

/// - `non_finalized` = records applied, encoded and readable, not yet fsynced (no second fold,
///   `docs/design/non-finalized-state.md`)
/// - `carry` = cumulative tree sizes after the last applied block (`durable`'s = after the last
///   committed one, what a reorg restores)
/// - `bulk` = final blocks not yet committed (never applied, encoded by the commit), `bulk_bytes`
///   their [`Weight`]
pub struct CompactBlockIndexWriter {
    store: Offloaded<CompactBlockStore>,
    durable: Durable,
    non_finalized: NonFinalizedState,
    carry: TreeSizes,
    bulk: Vec<(Arc<Block>, Arc<BlockFees>)>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
    published: Published<ReadView>,
}

/// What the store committed, pinned at a commit; `tip` = last committed block, inclusive
/// (`None` = empty)
struct Durable {
    tip: Option<BlockRef>,
    sizes: TreeSizes,
    snapshot: Arc<Snapshot>,
}

impl Durable {
    fn of(store: &CompactBlockStore) -> Self {
        Self {
            tip: store.finalized_tip(),
            sizes: store.sizes(),
            snapshot: store.reader().snapshot(),
        }
    }
}

impl CompactBlockIndexWriter {
    pub const NAME: &'static str = "compact_block";

    /// Opens over `store`, reseeding the carry from its manifest; `batch_bytes` = final blocks
    /// per bulk commit (one fsync)
    pub fn new(store: CompactBlockStore, batch_bytes: NonZeroUsize) -> Self {
        let durable = Durable::of(&store);
        let view = ReadView::new(NonFinalizedState::default(), Arc::clone(&durable.snapshot));
        Self {
            carry: durable.sizes,
            published: Published::new(view, durable.tip.map(|tip| tip.height)),
            durable,
            store: Offloaded::new(store),
            non_finalized: NonFinalizedState::default(),
            bulk: Vec::new(),
            bulk_bytes: 0,
            batch_bytes,
        }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.durable.tip
    }

    /// View, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<ReadView> {
        &self.published
    }

    fn applied_height(&self) -> Option<Height> {
        self.non_finalized.tip_height().or(self.durable.tip.map(|tip| tip.height))
    }

    /// Follows `blocks` and `fees` (value-balance's, step for step) through their `Shutdown` (a
    /// failure panics: its dropped queues fail the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>, mut fees: Subscription<BlockFees>) {
        loop {
            let block_step = blocks.next().await;
            let fee_step = fees.next().await;
            match (block_step, fee_step) {
                (
                    Step::Apply { height, finalized, data: block },
                    Step::Apply { height: fee_height, finalized: fee_finalized, data: fees },
                ) => {
                    assert_eq!(
                        (height, finalized),
                        (fee_height, fee_finalized),
                        "compact_block: fees out of step"
                    );
                    assert!(
                        fees.belongs_to(&block),
                        "compact_block: fees at {height} for another block"
                    );
                    if finalized {
                        // replay for an index behind this one: already on disk
                        if Some(height) <= self.durable.tip.map(|tip| tip.height) {
                            continue;
                        }
                        self.bulk_bytes += block.weight() + fees.weight();
                        self.bulk.push((block, fees));
                        self.published.merged(height);
                        if self.bulk_bytes >= self.batch_bytes.get() {
                            self.commit(height).await;
                        }
                    } else {
                        // bulk → tip: what bulk staged commits before the first apply builds on it
                        if let Some((last, _)) = self.bulk.last() {
                            self.commit(last.header().height).await;
                        }
                        let next = self.applied_height().map_or(Height::GENESIS, Height::next);
                        // gap = every later commitment tree silently mis-sized
                        assert_eq!(height, next, "compact_block: blocks must arrive contiguously");
                        let carry = self.carry.advance(&block).unwrap_or_else(|error| {
                            panic!("{} index at {height}: {error}", Self::NAME)
                        });
                        // encoded once, here: serving reads these bytes, and so does the commit
                        let record = encode_compact_block(&block, &fees, &carry);
                        self.carry = carry;
                        self.non_finalized.apply(height, block.header().hash.into(), record, carry);
                    }
                }
                (Step::Finalized { height }, Step::Finalized { height: fee_height }) => {
                    assert_eq!(height, fee_height, "compact_block: fee Finalized out of step");
                    self.commit(height).await;
                }
                (Step::Reorg, Step::Reorg) => {
                    assert!(self.bulk.is_empty(), "compact_block: reorg with bulk blocks staged");
                    // back to the durable tip (no disk read: the durable carry is pinned)
                    self.non_finalized = NonFinalizedState::default();
                    self.carry = self.durable.sizes;
                    self.publish();
                    self.published.reorged();
                }
                (Step::Shutdown, Step::Shutdown) => {
                    if let Some((last, _)) = self.bulk.last() {
                        self.commit(last.header().height).await;
                    }
                    return;
                }
                _ => panic!("compact_block: block and fee steps out of step"),
            }
            self.publish();
        }
    }

    /// Every final block through `through` → disk (bulk ones encoded here, applied ones already
    /// encoded), then applied records leave the non-finalized tier for the files
    async fn commit(&mut self, through: Height) {
        let bulk = std::mem::take(&mut self.bulk);
        self.bulk_bytes = 0;
        let mut next = self.durable.tip.map_or(Height::GENESIS, |tip| tip.height.next());
        let mut sizes = self.durable.sizes;

        let mut unencoded = Vec::with_capacity(bulk.len());
        for (block, fees) in bulk {
            assert_eq!(block.header().height, next, "compact_block: final blocks not contiguous");
            next = next.next();
            sizes = sizes.advance(&block).unwrap_or_else(|error| {
                panic!("{} index at {}: {error}", Self::NAME, block.header().height)
            });
            unencoded.push((block, fees, sizes));
        }
        let mut encoded: Vec<(Height, [u8; HASH], Bytes)> = Vec::new();
        while next <= through {
            let record = self.non_finalized.block(next);
            let record = record.unwrap_or_else(|| panic!("compact_block: {next} final, not held"));
            let hash = self.non_finalized.hash_at(next).expect("held with its record");
            sizes = self.non_finalized.sizes_at(next).expect("held with its record");
            encoded.push((next, hash, record));
            next = next.next();
        }
        assert_eq!(next.checked_sub(1), Some(through), "compact_block: final blocks short");

        let written = self
            .store
            .blocking(move |store| {
                for (block, fees, sizes) in unencoded {
                    let framed = encode_compact_block(&block, &fees, &sizes);
                    store.append(block.header().height, block.header().hash.into(), &framed)?;
                }
                for (height, hash, record) in encoded {
                    store.append(height, hash, &record)?;
                }
                store.commit(sizes)
            })
            .await;
        let store = self.store.get();
        if let Err(error) = written {
            error.commit_failed(Self::NAME, store.path());
        }
        self.durable = Durable::of(store);
        let durable = self.durable.tip.map(|tip| tip.height);
        assert_eq!(durable, Some(through), "compact_block: committed tip off the batch");
        // durable now: no second copy in RAM
        self.non_finalized.finalize_through(through);
        // empty non-finalized tier = applied == durable → the two carries agree (bulk never
        // applied, reaches `apply` only through here)
        if self.non_finalized.is_empty() {
            self.carry = self.durable.sizes;
        }
        // view first: a reader woken by the durable tip pins the view that includes it
        self.publish();
        self.published.durable(durable);
    }

    /// Non-finalized + durable as one value, taken at one consistent moment (a reader resolves
    /// both tiers from one load)
    fn publish(&self) {
        let view = ReadView::new(self.non_finalized.clone(), Arc::clone(&self.durable.snapshot));
        self.published.view(view, self.applied_height());
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use proptest::strategy::Strategy as _;
    use prost::Message as _;
    use tokio::{sync::watch, task::JoinHandle};
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        BlockHeader, CompactCiphertext, Fee, OrchardAction, OrchardData, SaplingData,
        SaplingOutput, Transaction, TransactionId, TransparentData, Zatoshis,
    };
    use zaino_proto::proto::compact_formats as cf;
    use zaino_sync::{BlockSink, FeeSink, Served};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{record::FRAME_HEADER, CompactBlockReader};

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Fee block `height`'s paying tx carries (distinct per height: a pairing slip = a wrong fee)
    fn fee_at(height: u32) -> u32 {
        1_000 * (height + 1)
    }

    /// Block at `height` (hash `[height; 32]`): coinbase, then one tx committing `sapling`
    /// outputs, `orchard` and `ironwood` actions
    fn block(height: u32, sapling: usize, orchard: usize, ironwood: usize) -> Arc<Block> {
        let out = SaplingOutput {
            cmu: [1u8; 32].into(),
            ephemeral_key: [2u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
        };
        let action = OrchardAction {
            nullifier: [4u8; 32].into(),
            cmx: [5u8; 32].into(),
            ephemeral_key: [6u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([7u8; CompactCiphertext::LENGTH]),
        };

        Arc::new(Block::new(
            BlockHeader::for_tests(
                height,
                [height as u8; 32],
                [height.wrapping_sub(1) as u8; 32],
                1_700_000_000 + height,
            ),
            vec![
                Transaction {
                    txid: TransactionId::from([0xcb; 32]),
                    transparent: TransparentData { coinbase: true, ..Default::default() },
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                },
                Transaction {
                    txid: TransactionId::from([height as u8; 32]),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: SaplingData { outputs: vec![out; sapling], ..Default::default() },
                    orchard: OrchardData {
                        actions: vec![action.clone(); orchard],
                        ..Default::default()
                    },
                    ironwood: OrchardData { actions: vec![action; ironwood], ..Default::default() },
                },
            ],
        ))
    }

    fn apply(finalized: bool, block: Arc<Block>) -> Step<Block> {
        Step::Apply { height: block.header().height, finalized, data: block }
    }

    /// Tree sizes the store holds at `height`
    fn stored_sizes(reader: &CompactBlockReader, height: u32) -> (u32, u32, u32) {
        let record = reader.pin().block(h(height)).expect("record");
        let meta = cf::CompactBlock::decode(&record[FRAME_HEADER..])
            .expect("decodes")
            .chain_metadata
            .expect("carries metadata");
        (
            meta.sapling_commitment_tree_size,
            meta.orchard_commitment_tree_size,
            meta.ironwood_commitment_tree_size,
        )
    }

    fn open(fs: &Arc<SimFs>) -> CompactBlockStore {
        CompactBlockStore::open(fs.clone(), Path::new("/cb"), NetworkType::Regtest).expect("open")
    }

    /// The index over `/cb` running as zainod runs it: a producer's block sink, value-balance's
    /// fee sink (fees stated per block: coinbase, then [`fee_at`]), and what it publishes
    struct Running {
        blocks: BlockSink,
        fees: FeeSink,
        served: Served<ReadView>,
        applied: watch::Receiver<Option<Height>>,
        durable: watch::Receiver<Option<Height>>,
        run: JoinHandle<()>,
    }

    impl Running {
        /// `batch_bytes` = 1: every final block writes as it arrives
        fn start(fs: &Arc<SimFs>) -> Self {
            let index = CompactBlockIndexWriter::new(open(fs), NonZeroUsize::MIN);
            let published = index.published();
            let (served, applied, durable) = (
                published.served(),
                published.subscribe_applied(),
                published.subscribe_finalized(),
            );
            let (mut blocks, mut fees) = (BlockSink::new("blocks"), FeeSink::new("fees"));
            let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
            let (block_sub, fee_sub) = (
                blocks.subscribe(CompactBlockIndexWriter::NAME, queue),
                fees.subscribe(CompactBlockIndexWriter::NAME, queue),
            );
            let run = tokio::spawn(index.run(block_sub, fee_sub));
            Self { blocks, fees, served, applied, durable, run }
        }

        /// `step` to the block sink, value-balance's copy of it to the fee sink
        async fn send(&mut self, step: Step<Block>) {
            let fees = match &step {
                Step::Apply { height, finalized, data } => {
                    let paid = Zatoshis::new(fee_at(u32::from(*height)).into()).expect("supply");
                    let fees = BlockFees {
                        height: *height,
                        hash: data.header().hash,
                        fees: vec![Fee::Coinbase, Fee::Paid(paid)],
                    };
                    Step::Apply { height: *height, finalized: *finalized, data: Arc::new(fees) }
                }
                Step::Finalized { height } => Step::Finalized { height: *height },
                Step::Reorg => Step::Reorg,
                Step::Shutdown => unreachable!("`stop` ends both sinks"),
            };
            self.blocks.send(step).await;
            self.fees.send(fees).await;
        }

        /// Both tips published at these heights (the view published with them)
        async fn settled(&mut self, applied: Option<Height>, durable: Option<Height>) {
            let tips = async {
                self.durable.wait_for(|at| *at == durable).await.expect("index alive");
                self.applied.wait_for(|at| *at == applied).await.expect("index alive");
            };
            let waited = tokio::time::timeout(Duration::from_secs(5), tips).await;
            let (now_applied, now_durable) = (*self.applied.borrow(), *self.durable.borrow());
            waited.unwrap_or_else(|_| {
                panic!("tips {now_applied:?} / {now_durable:?}, want {applied:?} / {durable:?}")
            });
        }

        /// Shutdown down both sinks, the index drained through it
        async fn stop(self) {
            self.blocks.shutdown();
            self.fees.shutdown();
            self.run.await.expect("followed through Shutdown");
        }
    }

    /// Tree sizes accumulate across bulk and tip blocks; a reopened index resumes the carry from
    /// the manifest, not zero; a reorg rewinds it to durable
    #[tokio::test]
    async fn tree_sizes_accumulate_and_survive_a_restart_and_a_reorg() {
        let fs = SimFs::new();
        let mut index = Running::start(&fs);
        let through = |n| Some(h(n));

        // 0, 1 final on arrival (bulk sync); 2 applied at the tip, not yet final
        index.send(apply(true, block(0, 2, 1, 0))).await;
        index.send(apply(true, block(1, 3, 2, 1))).await;
        index.send(apply(false, block(2, 0, 0, 4))).await;
        index.settled(through(2), through(1)).await;
        let view = index.served.pin_any();
        assert!(view.resident_block(h(1)).is_none(), "bulk sync never touches non-finalized");
        assert!(view.resident_block(h(2)).is_some(), "applied, not durable: non-finalized");

        index.send(Step::Finalized { height: h(2) }).await;
        index.settled(through(2), through(2)).await;
        let view = index.served.pin_any();
        assert!(view.resident_block(h(2)).is_none(), "written block leaves non-finalized");
        index.stop().await;
        let stored = [0, 1, 2].map(|height| stored_sizes(&open(&fs).reader(), height));
        assert_eq!(stored, [(2, 1, 0), (5, 3, 1), (5, 3, 5)], "cumulative, not per-block");

        // reopened: resumes at 2; a losing 3, a reorg, the winning 3 folds onto the right totals
        let mut resumed = Running::start(&fs);
        resumed.settled(through(2), through(2)).await;
        resumed.send(apply(false, block(3, 9, 9, 9))).await;
        resumed.settled(through(3), through(2)).await;
        resumed.send(Step::Reorg).await;
        resumed.settled(through(2), through(2)).await;
        let view = resumed.served.pin_any();
        assert!(view.resident_block(h(3)).is_none(), "losing branch gone");
        resumed.send(apply(false, block(3, 1, 1, 1))).await;
        resumed.send(Step::Finalized { height: h(3) }).await;
        resumed.settled(through(3), through(3)).await;
        resumed.stop().await;
        // carry survived restart + reorg: not zero, not the losing branch's 9s
        assert_eq!(stored_sizes(&open(&fs).reader(), 3), (6, 4, 6));
    }

    #[derive(Debug, Clone)]
    enum Move {
        Apply,
        Finalize(usize),
        Reorg,
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through a producer's random step sequences (bulk, tip applies,
        /// finalizes, reorgs with replay, restarts): after every move each applied height serves
        /// a record whose hash and cumulative tree sizes equal an independently summed model and
        /// whose fees are its own block's, and a full durable read is those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => proptest::strategy::Just(Move::Apply),
                    2 => (1usize..=4).prop_map(Move::Finalize),
                    1 => proptest::strategy::Just(Move::Reorg),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves));
        }
    }

    async fn random_history(counts: Vec<(usize, usize, usize)>, moves: Vec<Move>) {
        let chain: Vec<Arc<Block>> =
            (0u32..).zip(&counts).map(|(height, &(s, o, i))| block(height, s, o, i)).collect();
        let sizes_through = |height: usize| {
            counts[..=height].iter().fold((0u32, 0u32, 0u32), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };
        let tip = |count: usize| count.checked_sub(1).map(|last| h(last as u32));

        let fs = SimFs::new();
        let mut index = Running::start(&fs);
        // the producer's side: blocks final (= durable once written), blocks sent in all
        let (mut finals, mut sent) = (0usize, 0usize);
        for (at, step) in moves.iter().enumerate() {
            match *step {
                Move::Apply if sent < chain.len() => {
                    index.send(apply(false, Arc::clone(&chain[sent]))).await;
                    sent += 1;
                }
                // window open: its oldest become final; none: the next blocks arrive final (bulk)
                Move::Finalize(n) if sent > finals => {
                    let buried = (finals + n).min(sent);
                    for height in finals..buried {
                        index.send(Step::Finalized { height: h(height as u32) }).await;
                    }
                    finals = buried;
                }
                Move::Finalize(n) => {
                    for next in (sent..chain.len()).take(n) {
                        index.send(apply(true, Arc::clone(&chain[next]))).await;
                        (sent, finals) = (sent + 1, finals + 1);
                    }
                }
                // a producer reorgs only a window it opened, then replays from its first height
                Move::Reorg if sent > finals => {
                    index.send(Step::Reorg).await;
                    sent = finals;
                }
                Move::Reopen => {
                    index.stop().await;
                    index = Running::start(&fs);
                    sent = finals;
                }
                _ => {}
            }
            let case = format!("move {at} {step:?}");
            index.settled(tip(sent), tip(finals)).await;

            // every applied height served from one tier, durable records in order
            let view = index.served.pin_any();
            assert_eq!(view.tip(), tip(sent), "{case}");
            let mut expected_span = Vec::new();
            for height in tip(sent).into_iter().flat_map(|last| Height::GENESIS.up_to(last)) {
                let record =
                    view.block(height).unwrap_or_else(|| panic!("{case}: no record at {height}"));
                let decoded = cf::CompactBlock::decode(&record[FRAME_HEADER..]).expect("decodes");
                let meta = decoded.chain_metadata.expect("metadata");
                let n = u32::from(height);
                let sizes = (
                    meta.sapling_commitment_tree_size,
                    meta.orchard_commitment_tree_size,
                    meta.ironwood_commitment_tree_size,
                );
                let fees: Vec<_> = decoded.vtx.iter().map(|tx| tx.fee).collect();
                let record_case = format!("{case}: record {height}");
                assert_eq!(decoded.hash, [n as u8; 32].to_vec(), "{record_case}");
                assert_eq!(sizes, sizes_through(n as usize), "{record_case}");
                assert_eq!(fees, vec![0, fee_at(n)], "{record_case}: coinbase unset, its own fee");
                expected_span.extend_from_slice(&record);
            }
            if let Some(last) = tip(finals) {
                let (span, reach) =
                    view.span_from(Height::GENESIS, last, usize::MAX).expect("durable span");
                assert_eq!(reach, last, "{case}");
                let expected = &expected_span[..span.len()];
                assert_eq!(span.as_ref(), expected, "{case}: records in order");
            }
        }
        index.stop().await;
    }

    /// Gap in the delivered chain = every later block silently mis-sized → the index panics,
    /// never skips
    #[tokio::test]
    async fn a_gap_in_the_block_stream_panics() {
        let fs = SimFs::new();
        let mut index = Running::start(&fs);

        index.send(apply(false, block(0, 1, 1, 1))).await;
        index.send(apply(false, block(2, 1, 1, 1))).await;
        let panic = index.run.await.expect_err("panicked").into_panic();
        let message = panic.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.contains("blocks must arrive contiguously"), "{message}");
    }
}
