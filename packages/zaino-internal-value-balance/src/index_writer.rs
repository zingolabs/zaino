//! [`IndexWriter`]: each delivered block's outputs recorded, its inputs resolved, its balances
//! published
//!
//! - `deliver` does all three for its run (called before the harness stages or applies, bulk
//!   included: one item per block published, resolved a run at a time)
//! - `apply` only moves the nonfinalised extent
//! - `finalize` writes the outputs `deliver` recorded; they leave `pending` once it lands

use std::{collections::HashMap, path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs,
    lsm::{LsmStore, SegmentSet, Snapshot},
    manifest::Committed,
    StoreError,
};
use zaino_primitives::types::{
    Block, BlockHash, BlockValueBalances, Extent, Height, OutPoint, OutputIndex, TransactionId,
    ValueBalance, Zatoshis,
};
use zaino_sync::{blocking, IndexWriter, Offloaded, SinkGone, ValueBalanceSink};
use zcash_protocol::consensus::NetworkType;

use crate::{key::OutputRow, pending::Pending, ValueBalanceIndex};

#[derive(Debug, thiserror::Error)]
pub enum IndexWriterError {
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Spent outpoint this index never recorded (the chain is indexed from genesis: a bug or a
    /// foreign directory, never a gap to tolerate)
    #[error(
        "block {height} tx {txid}: spends {spent}:{vout}, an output this index never recorded"
    )]
    MissingPrevout { height: Height, txid: TransactionId, spent: TransactionId, vout: OutputIndex },

    #[error("block {height} tx {txid}: value sums past the money supply")]
    ValueOverflow { height: Height, txid: TransactionId },

    #[error(transparent)]
    Downstream(#[from] SinkGone),
}

/// Records every transparent output and publishes one [`BlockValueBalances`] per block
///
/// - `sink` starts where its subscribers' durable extents end; heights below it are recorded
///   but not published (downstream holds them)
/// - `committed` = the store's as of the last landing (answered without the store while a
///   write has it)
pub struct ValueBalanceIndexWriter {
    outputs: SegmentSet<OutPoint>,
    store: Offloaded<LsmStore<ValueBalanceIndex>>,
    committed: Committed,
    applied: Extent,
    pending: Pending,
    sink: ValueBalanceSink,
}

/// A finished `finalize` write: the store back, plus the outpoints it now holds
pub struct Landing {
    store: LsmStore<ValueBalanceIndex>,
    landed: Vec<OutPoint>,
}

impl ValueBalanceIndexWriter {
    /// Opens `path` at its committed state (every listed segment proven, every other one removed)
    pub fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        network: NetworkType,
        sink: ValueBalanceSink,
    ) -> Result<Self, IndexWriterError> {
        let store = LsmStore::open(fs, path, network)?;
        Ok(Self {
            committed: store.committed(),
            applied: store.committed().extent,
            outputs: store.sets(),
            store: Offloaded::new(store),
            pending: Pending::default(),
            sink,
        })
    }
}

/// Each of `blocks`' balances, every prevout found in `pending` (the whole run's outputs
/// included) or `durable`
///
/// - durable prevouts of the whole run resolved in one `get_many` (sorted keys, probed in
///   parallel: cold page faults overlap instead of queueing one block behind another)
/// - `pending` holding later blocks' outputs is harmless: no block spends an output created
///   after it
fn resolve(
    blocks: &[Arc<Block>],
    pending: &Pending,
    durable: &Snapshot<OutPoint>,
) -> Result<Vec<BlockValueBalances>, IndexWriterError> {
    let prevouts =
        blocks.iter().flat_map(|block| block.transactions()).flat_map(|tx| &tx.transparent.inputs);
    let mut values: HashMap<OutPoint, Zatoshis> = HashMap::new();
    let mut unheld = Vec::new();
    for prevout in prevouts {
        match pending.value(prevout) {
            Some(value) => _ = values.insert(*prevout, value),
            None => unheld.push(*prevout),
        }
    }
    let rows = durable.get_many::<OutputRow>(&unheld);
    values.extend(unheld.iter().zip(rows).filter_map(|(key, row)| Some((*key, row?.value))));

    blocks.iter().map(|block| balances(block, &values)).collect()
}

/// `block`'s balances, every prevout's value already in `values`
fn balances(
    block: &Block,
    values: &HashMap<OutPoint, Zatoshis>,
) -> Result<BlockValueBalances, IndexWriterError> {
    let height = block.header().height;
    let balances = block
        .transactions()
        .iter()
        .map(|tx| {
            let overflow = || IndexWriterError::ValueOverflow { height, txid: tx.txid };
            let spent =
                tx.transparent.inputs.iter().try_fold(Zatoshis::ZERO, |spent, prevout| {
                    let value = values.get(prevout).ok_or(IndexWriterError::MissingPrevout {
                        height,
                        txid: tx.txid,
                        spent: prevout.txid,
                        vout: prevout.vout,
                    })?;
                    spent.checked_add(*value).ok_or_else(overflow)
                })?;
            ValueBalance::of(tx, spent).map_err(|_| overflow())
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(BlockValueBalances { height, hash: block.header().hash, balances })
}

impl IndexWriter for ValueBalanceIndexWriter {
    type Input = Block;
    type View = ();
    type Error = IndexWriterError;
    type Done = Landing;

    const NAME: &'static str = "value_balance";

    fn finalized_height(&self) -> Extent {
        self.committed.extent
    }

    fn finalized_tip(&self) -> Option<BlockHash> {
        self.committed.tip
    }

    fn applied_height(&self) -> Extent {
        self.applied
    }

    fn view(&self) {}

    async fn deliver(&mut self, blocks: &[Arc<Block>]) -> Result<(), IndexWriterError> {
        // every output of the run first: a later block may spend an earlier one's
        for block in blocks {
            // durable = outputs already on disk (a replay for a downstream index behind this one)
            if !self.finalized_height().contains(block.header().height) {
                self.pending.insert(block);
            }
        }
        let next = self.sink.next();
        let unpublished: Vec<Arc<Block>> =
            blocks.iter().filter(|block| block.header().height >= next).cloned().collect();
        if unpublished.is_empty() {
            return Ok(());
        }

        // durable prevouts = segment probes (a cold page fault each): the blocking pool's step
        let (pending, durable) = (self.pending.clone(), self.outputs.pin());
        let resolved = blocking(move || resolve(&unpublished, &pending, &durable)).await?;
        for balances in resolved {
            self.sink.add(balances.height, Arc::new(balances)).await?;
        }
        Ok(())
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), IndexWriterError> {
        let height = block.header().height;
        assert_eq!(height, self.applied.next(), "value_balance: blocks must arrive contiguously");
        self.applied = Extent::through(height);
        Ok(())
    }

    async fn finalize(
        &mut self,
        blocks: &[Arc<Block>],
    ) -> Result<
        impl FnOnce() -> Result<Self::Done, IndexWriterError> + Send + 'static,
        IndexWriterError,
    > {
        let mut reached = self.finalized_height();
        for block in blocks {
            let height = block.header().height;
            assert_eq!(height, reached.next(), "value_balance: finalize batch not contiguous");
            reached = Extent::through(height);
        }
        let tip = blocks.last().expect("value_balance: finalize with no blocks").header().hash;

        let rows = self.pending.rows_below(reached);
        let landing: Vec<OutPoint> = rows.iter().map(|row| row.key).collect();
        let mut store = self.store.lend();
        Ok(move || {
            store.commit(rows, reached, tip)?;
            Ok(Landing { store, landed: landing })
        })
    }

    async fn committed(
        &mut self,
        Landing { store, landed }: Landing,
    ) -> Result<(), IndexWriterError> {
        self.committed = store.committed();
        self.store.restore(store);
        // durable segments answer for these now (published by the write, before this)
        self.pending.remove(&landed);

        let reached = self.committed.extent;
        self.applied = self.applied.max(reached);
        self.sink.finalize_through(reached).await?;
        Ok(())
    }

    async fn reset(&mut self) -> Result<(), IndexWriterError> {
        // segments untouched (commits are final-only); the harness flushed what was staged
        self.pending = Pending::default();
        self.applied = self.finalized_height();
        self.sink.finalize_through(self.applied).await?;
        self.sink.reset().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        BlockHeader, OrchardData, ReorgDepth, SaplingData, Script, SignedZatoshis, SproutData,
        Transaction, TransparentData, TransparentOutput,
    };
    use zaino_sync::{FollowError, IndexFollower, SinkBuilder, Subscription};

    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn depth(n: u32) -> ReorgDepth {
        ReorgDepth::new(NonZeroU32::new(n).expect("non-zero"))
    }

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    /// `[fork, height]`-tagged hash (a fork only differs where it is named)
    fn hash(height: u32, fork: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = fork;
        bytes[1..5].copy_from_slice(&height.to_be_bytes());
        bytes
    }

    /// Block `height` on `fork`, linked onto `parent`'s block at `height - 1`
    fn block(height: u32, fork: u8, parent: u8, txs: Vec<Transaction>) -> Arc<Block> {
        Arc::new(Block::new(
            BlockHeader::for_tests(
                height,
                hash(height, fork),
                hash(height.wrapping_sub(1), parent),
                1_700_000_000 + height,
            ),
            txs,
        ))
    }

    /// `(txid tag, spends, outputs, [sprout, sapling, orchard, ironwood] balances)`
    fn tx(tag: u8, spends: &[(u8, u32)], outputs: &[u64], shielded: [i64; 4]) -> Transaction {
        let signed = |value| SignedZatoshis::new(value).expect("in supply");
        Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                inputs: spends
                    .iter()
                    .map(|&(prev, vout)| OutPoint { txid: TransactionId::from([prev; 32]), vout })
                    .collect(),
                outputs: outputs
                    .iter()
                    .map(|&value| TransparentOutput {
                        value: Zatoshis::new(value).expect("in supply"),
                        script: Script::new(vec![0x51]),
                    })
                    .collect(),
            },
            sprout: SproutData { value_balance: signed(shielded[0]) },
            sapling: SaplingData { value_balance: signed(shielded[1]), ..Default::default() },
            orchard: OrchardData { value_balance: signed(shielded[2]), ..Default::default() },
            ironwood: OrchardData { value_balance: signed(shielded[3]), ..Default::default() },
        }
    }

    fn coinbase(tag: u8) -> Transaction {
        tx(tag, &[], &[625_000_000], [0; 4])
    }

    /// Per tx fee, in zats (`None` = coinbase)
    fn fees(balances: &BlockValueBalances) -> Vec<Option<u64>> {
        balances.balances.iter().map(|balance| balance.fee().map(Zatoshis::as_u64)).collect()
    }

    async fn next_for(
        balances: &mut Subscription<BlockValueBalances>,
        block: &Block,
    ) -> Arc<BlockValueBalances> {
        balances
            .balances_for(block)
            .await
            .unwrap_or_else(|| panic!("no balances for block {:?}", block.header().height))
    }

    /// Depth 2, tip 4 (0..=2 final, committed one per batch; 3, 4 nonfinalised): every prevout
    /// resolves wherever it lives
    /// - 1: spends 0's output (durable) and one from earlier in its own block (staged)
    /// - 3: spends 1's outputs (durable) with value leaving sprout
    /// - 4: spends 3's output (nonfinalised) and enters ironwood
    ///
    /// Restart with a downstream index durable at 0 (BlockSink starts there): 0..=2 replay
    /// through durable storage alone, 3.. are recorded again, and every item is identical
    ///
    /// Both batch sizes: 1 byte = one block per `deliver`; 1 MiB = the queued chain as one run
    /// (1's spend of 0's output then resolves inside the run, not from a committed segment)
    #[tokio::test]
    async fn balances_resolve_every_prevout_wherever_it_lives_and_a_replay_republishes_them() {
        for batch in [NonZeroUsize::MIN, QUEUE] {
            resolve_every_prevout_and_replay(batch).await;
        }
    }

    async fn resolve_every_prevout_and_replay(batch: NonZeroUsize) {
        let chain = [
            block(0, 0, 0, vec![tx(0x10, &[], &[100_000], [0; 4])]),
            block(
                1,
                0,
                0,
                vec![
                    coinbase(0x11),
                    tx(0x21, &[(0x10, 0)], &[60_000, 39_000], [0; 4]),
                    tx(0x22, &[(0x21, 1)], &[30_000], [0, -8_000, 0, 0]),
                ],
            ),
            block(2, 0, 0, vec![coinbase(0x12), tx(0x23, &[(0x21, 0)], &[], [0, 0, -59_000, 0])]),
            block(
                3,
                0,
                0,
                vec![
                    coinbase(0x13),
                    tx(0x24, &[(0x22, 0), (0x11, 0)], &[625_029_500], [500, 0, 0, 0]),
                ],
            ),
            block(
                4,
                0,
                0,
                vec![coinbase(0x14), tx(0x25, &[(0x24, 0)], &[625_000_000], [0, 0, 0, -29_000])],
            ),
        ];
        let expected_fees = [
            vec![None],
            vec![None, Some(1_000), Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(500)],
        ];
        let fs = SimFs::new();

        let mut first_boot = Vec::new();
        for (boot, downstream) in [("first", Extent::ZERO), ("restart", Extent::through(h(0)))] {
            let mut balances = SinkBuilder::<BlockValueBalances>::new(depth(2));
            let mut consumer = balances.subscribe("consumer", QUEUE, downstream);
            let writer = ValueBalanceIndexWriter::open(
                fs.clone(),
                Path::new("/vb"),
                NetworkType::Regtest,
                balances.seal(),
            )
            .expect("open");
            let durable = writer.finalized_height();
            let mut blocks = SinkBuilder::<Block>::new(depth(2));
            let subscription = blocks.subscribe("value_balance", QUEUE, durable);
            let follower = IndexFollower::new(writer, subscription, batch);
            let _downstream = blocks.subscribe("downstream", QUEUE, downstream);
            let finalized = follower.subscribe_finalized();
            let mut sink = blocks.seal();
            let running = tokio::spawn(follower.run());

            sink.set_tip(h(4)).await.expect("tip");
            for block in &chain[u32::from(sink.next()) as usize..] {
                sink.add(block.header().height, Arc::clone(block)).await.expect("queued");
            }

            let mut published = Vec::new();
            for block in &chain[u32::from(downstream.next()) as usize..] {
                published.push(next_for(&mut consumer, block).await);
            }
            drop(sink);
            running.await.expect("joined").expect("clean stop");

            match boot {
                "first" => {
                    assert_eq!(durable, Extent::ZERO, "fresh directory");
                    let published_fees: Vec<_> = published.iter().map(|b| fees(b)).collect();
                    assert_eq!(published_fees, expected_fees, "fees per tx");
                    first_boot = published;
                }
                _ => {
                    assert_eq!(durable, Extent::through(h(2)), "0..=2 committed");
                    assert_eq!(published, first_boot[1..], "replay republishes identical balances");
                }
            }
            let finalized = *finalized.borrow();
            assert_eq!(finalized, Extent::through(h(2)), "{boot}: final durable, nonfinalised not");
        }
    }

    /// Depth 2, tip 3 on fork 0 (2, 3 nonfinalised), then fork 1 wins from 2 (its 3 spends an
    /// output only its own 2 created): the consumer, pairing late, skips fork 0's queued items and
    /// gets fork 1's, resolved against fork 1's outputs
    #[tokio::test]
    async fn a_reorg_drops_the_losing_branch_outputs_and_republishes_the_winner() {
        let genesis = block(0, 0, 0, vec![tx(0x10, &[], &[100_000], [0; 4])]);
        let one = block(1, 0, 0, vec![coinbase(0x11)]);
        let losing = [
            block(2, 0, 0, vec![coinbase(0x12), tx(0x20, &[(0x10, 0)], &[99_000], [0; 4])]),
            block(3, 0, 0, vec![coinbase(0x13), tx(0x30, &[(0x20, 0)], &[98_000], [0; 4])]),
        ];
        let winning = [
            block(2, 1, 0, vec![coinbase(0x42), tx(0x60, &[(0x10, 0)], &[90_000], [0; 4])]),
            block(3, 1, 1, vec![coinbase(0x43), tx(0x70, &[(0x60, 0)], &[80_000], [0; 4])]),
        ];

        let mut balances = SinkBuilder::<BlockValueBalances>::new(depth(2));
        let mut consumer = balances.subscribe("consumer", QUEUE, Extent::ZERO);
        let writer = ValueBalanceIndexWriter::open(
            SimFs::new(),
            Path::new("/vb"),
            NetworkType::Regtest,
            balances.seal(),
        )
        .expect("open");
        let mut blocks = SinkBuilder::<Block>::new(depth(2));
        let subscription = blocks.subscribe("value_balance", QUEUE, Extent::ZERO);
        let follower = IndexFollower::new(writer, subscription, NonZeroUsize::MIN);
        let mut sink = blocks.seal();
        let running = tokio::spawn(follower.run());

        sink.set_tip(h(3)).await.expect("tip");
        for block in [&genesis, &one].into_iter().chain(&losing) {
            sink.add(block.header().height, Arc::clone(block)).await.expect("queued");
        }
        assert_eq!(sink.reset().await.expect("reset"), h(2));
        for block in &winning {
            sink.add(block.header().height, Arc::clone(block)).await.expect("queued");
        }

        let paired: Vec<_> = [&genesis, &one]
            .into_iter()
            .chain(&winning)
            .map(|block| (u32::from(block.header().height), block))
            .collect();
        for (height, block) in paired {
            let balances = next_for(&mut consumer, block).await;
            assert_eq!(balances.hash, block.header().hash, "{height}: its own branch");
            let expected = match height {
                0 => vec![None],
                1 => vec![None],
                2 => vec![None, Some(10_000)],
                _ => vec![None, Some(10_000)],
            };
            assert_eq!(fees(&balances), expected, "{height}");
        }
        drop(sink);
        running.await.expect("joined").expect("clean stop");
    }

    /// A spend of an outpoint never recorded (a foreign directory, a gap): fatal, both txids named
    #[tokio::test]
    async fn a_spend_of_an_unrecorded_output_stops_the_index() {
        let mut balances = SinkBuilder::<BlockValueBalances>::new(depth(2));
        let _consumer = balances.subscribe("consumer", QUEUE, Extent::ZERO);
        let writer = ValueBalanceIndexWriter::open(
            SimFs::new(),
            Path::new("/vb"),
            NetworkType::Regtest,
            balances.seal(),
        )
        .expect("open");
        let mut blocks = SinkBuilder::<Block>::new(depth(2));
        let subscription = blocks.subscribe("value_balance", QUEUE, Extent::ZERO);
        let follower = IndexFollower::new(writer, subscription, NonZeroUsize::MIN);
        let mut sink = blocks.seal();
        let running = tokio::spawn(follower.run());

        sink.set_tip(h(0)).await.expect("tip");
        let orphan = block(0, 0, 0, vec![coinbase(0x10), tx(0x20, &[(0x99, 3)], &[1], [0; 4])]);
        sink.add(h(0), orphan).await.expect("queued");

        let stopped = running.await.expect("joined");
        let Err(FollowError::Index { index, source }) = &stopped else { panic!("{stopped:?}") };
        let IndexWriterError::MissingPrevout { height, txid, spent, vout } = source else {
            panic!("{source:?}")
        };
        let id = |byte| TransactionId::from([byte; 32]);
        assert_eq!(*index, "value_balance");
        assert_eq!((*height, *txid, *spent, *vout), (h(0), id(0x20), id(0x99), 3));
    }
}
