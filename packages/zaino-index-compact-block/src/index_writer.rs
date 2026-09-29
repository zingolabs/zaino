//! [`IndexWriter`]: raw blocks → framed records, plus the commitment-tree sizes they carry
//!
//! - only place tree sizes are computed ([`Block`] carries none; `z_gettreestate` = one round trip
//!   per block, unaffordable in a full sync)
//! - derived: size at `h` = size at `h - 1` + what `h` commits → block order non-negotiable (a gap
//!   silently mis-sizes every later block; [`apply`](CompactBlockIndexWriter::apply) asserts first)
//! - resume seeds the carry from the manifest (sizes committed with the tip)
//! - fees: each block's [`BlockValueBalances`] pulled from the value-balance sink in `deliver`
//!   (paired by hash), held until the block is encoded

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use zaino_persistence::StoreError;
use zaino_primitives::types::{
    Block, BlockHash, BlockValueBalances, Extent, Height, TreeSizeOutOfRange, TreeSizes,
};
use zaino_sync::{IndexWriter, Offloaded, Subscription};

use crate::{encode_compact_block, CompactBlockStore, NonFinalizedState, ReadView, Snapshot, HASH};

#[derive(Debug, thiserror::Error)]
pub enum IndexWriterError {
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Pool's cumulative size left the compact protocol's `u32` range (#549)
    #[error("commitment tree size out of range: {0}")]
    TreeSize(#[from] TreeSizeOutOfRange),

    /// Value-balance index stopped publishing (it failed; zainod reports its own error)
    #[error("value-balance stream closed before block {height}'s balances arrived")]
    BalancesGone { height: Height },
}

/// - `non_finalized` = records applied, encoded and readable, not yet fsynced (no second fold,
///   `docs/design/precommit-state.md`)
/// - `carry` = cumulative tree sizes after the last applied block (`durable`'s = after the last
///   finalised one, what [`reset`](IndexWriter::reset) restores)
/// - `durable` = the store as of the last landing (answered without the store while a write has
///   it)
pub struct CompactBlockIndexWriter {
    store: Offloaded<CompactBlockStore>,
    durable: Durable,
    non_finalized: NonFinalizedState,
    carry: TreeSizes,
    balances: Subscription<BlockValueBalances>,
    /// Delivered blocks' balances, taken when the block is encoded (`apply`, or `finalize` for
    /// one never applied)
    resolved: BTreeMap<Height, Arc<BlockValueBalances>>,
}

/// What the store committed, pinned at a landing
struct Durable {
    extent: Extent,
    tip: Option<BlockHash>,
    sizes: TreeSizes,
    snapshot: Arc<Snapshot>,
}

impl Durable {
    fn of(store: &CompactBlockStore) -> Self {
        Self {
            extent: store.finalized_height(),
            tip: store.tip_hash(),
            sizes: store.sizes(),
            snapshot: store.reader().snapshot(),
        }
    }
}

/// A finished `finalize` write: the store back
pub struct Landing {
    store: CompactBlockStore,
}

/// One finalised block's record: encoded at `apply`, or encoded by the write
enum Pending {
    Encoded(Bytes),
    Unencoded { block: Arc<Block>, balances: Arc<BlockValueBalances>, sizes: TreeSizes },
}

impl CompactBlockIndexWriter {
    /// Opens over `store`, reseeding the carry from its manifest
    ///
    /// - `balances` subscribed at this store's durable extent (`ValueBalanceSink`)
    pub fn new(store: CompactBlockStore, balances: Subscription<BlockValueBalances>) -> Self {
        Self {
            carry: store.sizes(),
            durable: Durable::of(&store),
            store: Offloaded::new(store),
            non_finalized: NonFinalizedState::default(),
            balances,
            resolved: BTreeMap::new(),
        }
    }

    fn take_balances(&mut self, height: Height) -> Arc<BlockValueBalances> {
        let delivered = self.resolved.remove(&height);
        delivered.unwrap_or_else(|| panic!("compact_block: {height} encoded before delivered"))
    }
}

impl IndexWriter for CompactBlockIndexWriter {
    type Input = Block;
    type View = ReadView;
    type Error = IndexWriterError;
    type Done = Landing;

    const NAME: &'static str = "compact_block";

    fn finalized_height(&self) -> Extent {
        self.durable.extent
    }

    fn finalized_tip(&self) -> Option<BlockHash> {
        self.durable.tip
    }

    fn applied_height(&self) -> Extent {
        self.non_finalized.tip_height().map_or(self.finalized_height(), Extent::through)
    }

    /// Nonfinalised + durable as one value, taken at one consistent moment (a reader resolves
    /// both tiers from one load)
    fn view(&self) -> ReadView {
        ReadView::new(self.non_finalized.clone(), Arc::clone(&self.durable.snapshot))
    }

    async fn deliver(&mut self, blocks: &[Arc<Block>]) -> Result<(), IndexWriterError> {
        for block in blocks {
            let height = block.header().height;
            // durable = already encoded (the sink starts past it, so nothing to pull)
            if self.finalized_height().contains(height) {
                continue;
            }
            let balances = self
                .balances
                .balances_for(block)
                .await
                .ok_or(IndexWriterError::BalancesGone { height })?;
            self.resolved.insert(height, balances);
        }
        Ok(())
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), IndexWriterError> {
        let height = block.header().height;
        // gap = every later commitment tree silently mis-sized
        let next = self.applied_height().next();
        assert_eq!(height, next, "compact_block: blocks must arrive contiguously");

        let balances = self.take_balances(height);
        let carry = self.carry.advance(block)?;
        // encoded once, here: serving reads these bytes, and so does the commit
        let record = encode_compact_block(block, &balances, &carry);
        self.carry = carry;
        self.non_finalized.apply(height, block.header().hash.into(), record);

        Ok(())
    }

    /// Blocks never applied (bulk) are encoded by the write, off the follower
    async fn finalize(
        &mut self,
        blocks: &[Arc<Block>],
    ) -> Result<impl FnOnce() -> Result<Landing, IndexWriterError> + Send + 'static, IndexWriterError>
    {
        let mut reached = self.finalized_height();
        let mut sizes = self.durable.sizes;
        let mut records = Vec::with_capacity(blocks.len());
        for block in blocks {
            let height = block.header().height;
            assert_eq!(height, reached.next(), "compact_block: finalised blocks not contiguous");
            reached = Extent::through(height);
            sizes = sizes.advance(block)?;

            let hash: [u8; HASH] = block.header().hash.into();
            // applied block = already encoded (same carry, its balances consumed by `apply`)
            let record = match self.non_finalized.block(height) {
                Some(record) => {
                    let held = self.non_finalized.hash_at(height);
                    assert_eq!(held, Some(hash), "compact_block: {height} over another branch");
                    Pending::Encoded(record)
                }
                None => Pending::Unencoded {
                    block: Arc::clone(block),
                    balances: self.take_balances(height),
                    sizes,
                },
            };
            records.push((height, hash, record));
        }

        let mut store = self.store.lend();
        Ok(move || {
            for (height, hash, record) in records {
                let framed = match record {
                    Pending::Encoded(record) => record,
                    Pending::Unencoded { block, balances, sizes } => {
                        encode_compact_block(&block, &balances, &sizes)
                    }
                };
                store.append(height, hash, &framed)?;
            }
            store.commit(sizes)?;
            Ok(Landing { store })
        })
    }

    async fn committed(&mut self, Landing { store }: Landing) -> Result<(), IndexWriterError> {
        self.durable = Durable::of(&store);
        self.store.restore(store);

        // durable now: no second copy in RAM
        if let Some(through) = self.durable.extent.last() {
            self.non_finalized.finalize_through(through);
        }

        // empty nonfinalised tier = `applied_height == finalized_height` → the two carries must
        // agree (bulk sync, never applied, reaches `apply` again only through here)
        if self.non_finalized.is_empty() {
            self.carry = self.durable.sizes;
        }

        Ok(())
    }

    async fn reset(&mut self) -> Result<(), IndexWriterError> {
        self.non_finalized = NonFinalizedState::default();
        // the harness flushed what was staged: anything left belonged to the dropped branch
        self.resolved.clear();
        // no disk read: the durable carry = what the applied carry returns to
        self.carry = self.durable.sizes;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::num::NonZeroUsize;

    use proptest::strategy::Strategy as _;
    use prost::Message as _;
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        BlockHeader, CompactCiphertext, OrchardAction, OrchardData, ReorgDepth, SaplingData,
        SaplingOutput, SignedZatoshis, Transaction, TransactionId, ValueBalance,
    };
    use zaino_proto::proto::compact_formats as cf;
    use zaino_sync::{ValueBalanceSink, ValueBalanceSinkBuilder};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{record::FRAME_HEADER, CompactBlockReader};

    /// Fee the feed prices every tx of block `height` at (a pairing slip shows as a wrong fee)
    fn fee_at(height: u32) -> u32 {
        1_000 * (height + 1)
    }

    /// Writer over `/cb` + the value-balance stream it pulls from, both at the store's durable
    /// extent (the sink stands in for the value-balance index)
    fn writer(fs: &Arc<SimFs>) -> (CompactBlockIndexWriter, ValueBalanceSink) {
        let store = open(fs);
        let mut balances = ValueBalanceSinkBuilder::new(ReorgDepth::CONSENSUS);
        let subscription = balances.subscribe(
            "compact_block",
            NonZeroUsize::new(1 << 20).expect("non-zero"),
            store.finalized_height(),
        );
        (CompactBlockIndexWriter::new(store, subscription), balances.seal())
    }

    /// Publishes `block`'s balances, then delivers it (the harness's order)
    async fn deliver(
        writer: &mut CompactBlockIndexWriter,
        feed: &mut ValueBalanceSink,
        block: &Arc<Block>,
    ) {
        let height = block.header().height;
        let fee = i64::from(fee_at(u32::from(height)));
        let balances = BlockValueBalances {
            height,
            hash: block.header().hash,
            balances: block
                .transactions()
                .iter()
                .map(|_| ValueBalance {
                    sapling: SignedZatoshis::new(fee).expect("in supply"),
                    ..Default::default()
                })
                .collect(),
        };
        feed.add(height, Arc::new(balances)).await.expect("fed");
        writer.deliver(std::slice::from_ref(block)).await.expect("deliver");
    }

    /// The harness's reset: the value-balance index rewinds its stream to its durable tip too
    async fn reset(writer: &mut CompactBlockIndexWriter, feed: &mut ValueBalanceSink) {
        writer.reset().await.expect("reset");
        feed.finalize_through(writer.finalized_height()).await.expect("fed");
        feed.reset().await.expect("fed");
    }

    /// Block at `height` committing `sapling` outputs, `orchard` and `ironwood` actions
    fn block(height: u32, sapling: usize, orchard: usize, ironwood: usize) -> Block {
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

        Block::new(
            BlockHeader::for_tests(
                height,
                [height as u8; 32],
                [height.wrapping_sub(1) as u8; 32],
                1_700_000_000 + height,
            ),
            vec![Transaction {
                txid: TransactionId::from([height as u8; 32]),
                transparent: Default::default(),
                sprout: Default::default(),
                sapling: SaplingData { outputs: vec![out; sapling], ..Default::default() },
                orchard: OrchardData {
                    actions: vec![action.clone(); orchard],
                    ..Default::default()
                },
                ironwood: OrchardData { actions: vec![action; ironwood], ..Default::default() },
            }],
        )
    }

    /// Tree sizes the index stored at `height`
    fn stored_sizes(reader: &CompactBlockReader, height: u32) -> (u32, u32, u32) {
        let height = Height::try_from(height).expect("height");
        let record = reader.pin().block(height).expect("record");
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

    /// Tree sizes accumulate across blocks; a reopened writer resumes the carry from the manifest,
    /// not zero
    #[tokio::test]
    async fn tree_sizes_accumulate_and_survive_a_restart() {
        let fs = SimFs::new();

        let (mut writer, mut feed) = writer(&fs);
        assert_eq!(writer.applied_height(), Extent::ZERO);
        assert_eq!(writer.finalized_tip(), None);

        // 0, 1 straight to `finalize` (bulk-sync path, below the reorg bound); 2 via `apply` first
        let bulk: Vec<Arc<Block>> = [(0u32, 2, 1, 0), (1, 3, 2, 1)]
            .into_iter()
            .map(|(height, s, o, i)| Arc::new(block(height, s, o, i)))
            .collect();
        for block in &bulk {
            deliver(&mut writer, &mut feed, block).await;
        }
        zaino_sync::finalize_now(&mut writer, &bulk).await.expect("finalize");
        assert!(!writer.view().has_non_finalized(), "bulk sync never touches nonfinalised");
        assert_eq!(writer.finalized_tip(), Some(BlockHash::from([1; 32])));

        let two = Arc::new(block(2, 0, 0, 4));
        deliver(&mut writer, &mut feed, &two).await;
        writer.apply(&two).await.expect("apply");
        assert_eq!(writer.applied_height(), Extent::counted(3));
        assert_eq!(writer.finalized_height(), Extent::counted(2), "applied is not durable");

        zaino_sync::finalize_now(&mut writer, &[two]).await.expect("finalize");
        assert!(!writer.view().has_non_finalized(), "finalised block leaves nonfinalised");

        let reader = writer.store.get().reader();
        let stored = [0, 1, 2].map(|height| stored_sizes(&reader, height));
        assert_eq!(stored, [(2, 1, 0), (5, 3, 1), (5, 3, 5)], "cumulative, not per-block");

        drop((writer, feed));
        let (mut resumed, mut feed) = self::writer(&fs);
        assert_eq!(resumed.applied_height(), Extent::counted(3), "resumes where it stopped");
        assert_eq!(resumed.finalized_tip(), Some(BlockHash::from([2; 32])));

        // losing branch at 3, then a reorg: `reset` drops it, rewinds the carry to durable (no
        // disk read) → the winning branch folds onto the right totals
        let losing = Arc::new(block(3, 9, 9, 9));
        deliver(&mut resumed, &mut feed, &losing).await;
        resumed.apply(&losing).await.expect("losing");
        reset(&mut resumed, &mut feed).await;
        assert!(!resumed.view().has_non_finalized(), "losing branch gone");
        assert_eq!(resumed.applied_height(), resumed.finalized_height());

        let winning = Arc::new(block(3, 1, 1, 1));
        deliver(&mut resumed, &mut feed, &winning).await;
        resumed.apply(&winning).await.expect("apply");
        zaino_sync::finalize_now(&mut resumed, &[winning]).await.expect("finalize");
        // carry survived restart + reorg: not zero, not the losing branch's 9s
        assert_eq!(stored_sizes(&resumed.store.get().reader(), 3), (6, 4, 6));
    }

    #[derive(Debug, Clone)]
    enum Step {
        Apply,
        Finalize(usize),
        Reset,
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through random apply / finalize / reset / reopen sequences: after every
        /// step each applied height serves a record whose hash and cumulative tree sizes equal an
        /// independently summed model and whose fees are its own block's, and a full-range read
        /// is those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            steps in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => proptest::strategy::Just(Step::Apply),
                    2 => (1usize..=4).prop_map(Step::Finalize),
                    1 => proptest::strategy::Just(Step::Reset),
                    1 => proptest::strategy::Just(Step::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(random_history(counts, steps));
        }
    }

    async fn random_history(counts: Vec<(usize, usize, usize)>, steps: Vec<Step>) {
        let chain: Vec<Arc<Block>> = (0u32..)
            .zip(&counts)
            .map(|(height, &(s, o, i))| Arc::new(block(height, s, o, i)))
            .collect();
        let sizes_through = |height: usize| {
            counts[..=height].iter().fold((0u32, 0u32, 0u32), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };

        // every applied height served from one tier, records in durable order
        let check = |writer: &CompactBlockIndexWriter, case: &str| {
            let view = writer.view();
            let applied = writer.applied_height();
            assert_eq!(view.tip(), applied, "{case}");
            let mut expected_span = Vec::new();
            for height in applied.last().into_iter().flat_map(|last| Height::GENESIS.up_to(last)) {
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
                assert_eq!(fees, vec![fee_at(n)], "{record_case}: its own fees");
                expected_span.extend_from_slice(&record);
            }

            let finalized = writer.finalized_height();
            if let Some(last) = finalized.last() {
                let (span, reach) =
                    view.span_from(Height::GENESIS, last, usize::MAX).expect("durable span");
                assert_eq!(reach, finalized, "{case}");
                let expected = &expected_span[..span.len()];
                assert_eq!(span.as_ref(), expected, "{case}: records in order");
            }
        };

        let fs = SimFs::new();
        let (mut writer, mut feed) = writer(&fs);
        for (at, step) in steps.iter().enumerate() {
            let (applied, finalized) = (
                u64::from(writer.applied_height()) as usize,
                u64::from(writer.finalized_height()) as usize,
            );
            match *step {
                Step::Apply if applied < chain.len() => {
                    deliver(&mut writer, &mut feed, &chain[applied]).await;
                    writer.apply(&chain[applied]).await.expect("apply");
                }
                Step::Finalize(count) if finalized < chain.len() => {
                    let end = (finalized + count).min(chain.len());
                    // applied ones delivered then, the rest now (bulk: final on arrival)
                    for block in &chain[applied.clamp(finalized, end)..end] {
                        deliver(&mut writer, &mut feed, block).await;
                    }
                    let write = writer.finalize(&chain[finalized..end]).await.expect("finalize");
                    let done = write().expect("written");
                    check(&writer, &format!("step {at} {step:?}, written, not landed"));
                    writer.committed(done).await.expect("landed");
                }
                Step::Reset => reset(&mut writer, &mut feed).await,
                Step::Reopen => {
                    drop((writer, feed));
                    (writer, feed) = self::writer(&fs);
                }
                _ => {}
            }
            check(&writer, &format!("step {at} {step:?}"));
        }
    }

    /// Gap = every later block silently mis-sized → panic, never a skip
    #[tokio::test]
    #[should_panic(expected = "blocks must arrive contiguously")]
    async fn a_gap_in_the_block_stream_panics() {
        let fs = SimFs::new();
        let (mut writer, mut feed) = writer(&fs);

        let zero = Arc::new(block(0, 1, 1, 1));
        deliver(&mut writer, &mut feed, &zero).await;
        writer.apply(&zero).await.expect("apply");
        let _ = writer.apply(&Arc::new(block(2, 1, 1, 1))).await;
    }
}
