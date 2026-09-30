//! [`IndexWriter`]: each delivered block's outputs recorded, its inputs resolved into its fees
//!
//! - `deliver` records the run's outputs; [`Derives::derive`] resolves its fees (the follower
//!   forwards them, one per block, into the [`FeeSink`](zaino_sync::FeeSink))
//! - `apply` only moves the non-finalized extent
//! - `finalize` writes the outputs `deliver` recorded; they leave `pending` once it lands

use std::{collections::HashMap, path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs,
    lsm::{LsmStore, SegmentSet, Snapshot},
    manifest::Committed,
    StoreError,
};
use zaino_primitives::types::{
    Block, BlockFees, BlockRef, Fee, Height, OutPoint, OutputIndex, Transaction, TransactionId,
    Zatoshis,
};
use zaino_sync::{blocking, Derives, IndexWriter, Offloaded};
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

    #[error("block {height} tx {txid}: takes more from the transparent pool than it puts in")]
    NegativeFee { height: Height, txid: TransactionId },
}

/// Records every transparent output and derives one [`BlockFees`] per block
///
/// - `committed` = the store's as of the last landing (answered without the store while a
///   write has it)
/// - `applied` = last applied height, inclusive (`None` = none)
pub struct ValueBalanceIndexWriter {
    outputs: SegmentSet<OutPoint>,
    store: Offloaded<LsmStore<ValueBalanceIndex>>,
    committed: Committed,
    applied: Option<Height>,
    pending: Pending,
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
    ) -> Result<Self, IndexWriterError> {
        let store = LsmStore::open(fs, path, network)?;
        Ok(Self {
            committed: store.committed(),
            applied: store.committed().height(),
            outputs: store.sets(),
            store: Offloaded::new(store),
            pending: Pending::default(),
        })
    }
}

/// Each of `blocks`' fees, every prevout found in `pending` (the whole run's outputs
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
) -> Result<Vec<BlockFees>, IndexWriterError> {
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

    blocks
        .iter()
        .map(|block| {
            let height = block.header().height;
            let fees = block
                .transactions()
                .iter()
                .map(|tx| fee(height, tx, &values))
                .collect::<Result<_, _>>()?;
            Ok(BlockFees { height, hash: block.header().hash, fees })
        })
        .collect()
}

/// `tx`'s value left in the transparent transaction value pool (protocol.pdf#transactions §3.4)
///
/// - Σ transparent inputs − Σ transparent outputs + each shielded pool's value balance
fn fee(
    height: Height,
    tx: &Transaction,
    values: &HashMap<OutPoint, Zatoshis>,
) -> Result<Fee, IndexWriterError> {
    if tx.transparent.coinbase {
        return Ok(Fee::Coinbase);
    }
    let overflow = || IndexWriterError::ValueOverflow { height, txid: tx.txid };
    let spent = tx.transparent.inputs.iter().try_fold(Zatoshis::ZERO, |spent, prevout| {
        let value = values.get(prevout).ok_or(IndexWriterError::MissingPrevout {
            height,
            txid: tx.txid,
            spent: prevout.txid,
            vout: prevout.vout,
        })?;
        spent.checked_add(*value).ok_or_else(overflow)
    })?;
    let paid = Zatoshis::sum_balances(tx.transparent.outputs.iter().map(|out| out.value))
        .ok_or_else(overflow)?;

    // every term within ±MAX_MONEY (zip-0209) → Σ of six fits i64
    let remaining = spent.as_i64() - paid.as_i64()
        + i64::from(tx.sprout.value_balance)
        + i64::from(tx.sapling.value_balance)
        + i64::from(tx.orchard.value_balance)
        + i64::from(tx.ironwood.value_balance);
    // MUST be nonnegative (protocol.pdf#transactions §3.4 consensus rule)
    let remaining = u64::try_from(remaining)
        .map_err(|_| IndexWriterError::NegativeFee { height, txid: tx.txid })?;
    Ok(Fee::Paid(Zatoshis::new(remaining).map_err(|_| overflow())?))
}

impl IndexWriter for ValueBalanceIndexWriter {
    type Input = Block;
    type View = ();
    type Error = IndexWriterError;
    type Done = Landing;

    const NAME: &'static str = "value_balance";

    fn finalized_tip(&self) -> Option<BlockRef> {
        self.committed.tip
    }

    fn applied_height(&self) -> Option<Height> {
        self.applied
    }

    fn view(&self) {}

    /// Every output of the run first: a later block may spend an earlier one's
    async fn deliver(&mut self, blocks: &[Arc<Block>]) -> Result<(), IndexWriterError> {
        let durable = self.finalized_height();
        for block in blocks {
            // durable = outputs already on disk (a replay for a downstream index behind this one)
            if Some(block.header().height) > durable {
                self.pending.insert(block);
            }
        }
        Ok(())
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), IndexWriterError> {
        let height = block.header().height;
        let next = self.applied.map_or(Height::GENESIS, Height::next);
        assert_eq!(height, next, "value_balance: blocks must arrive contiguously");
        self.applied = Some(height);
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
            let next = reached.map_or(Height::GENESIS, Height::next);
            assert_eq!(height, next, "value_balance: finalize batch not contiguous");
            reached = Some(height);
        }
        let last = blocks.last().expect("value_balance: finalize with no blocks").header();
        let tip = BlockRef { hash: last.hash, height: last.height };

        let rows = self.pending.rows_through(reached);
        let landing: Vec<OutPoint> = rows.iter().map(|row| row.key).collect();
        let mut store = self.store.lend();
        Ok(move || {
            store.commit(rows, tip)?;
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

        self.applied = self.applied.max(self.committed.height());
        Ok(())
    }

    async fn reset(&mut self) -> Result<(), IndexWriterError> {
        // segments untouched (commits are final-only); the harness flushed what was staged
        self.pending = Pending::default();
        self.applied = self.finalized_height();
        Ok(())
    }

    fn wants_commit(&self) -> bool {
        self.store.get().merge_finished()
    }
}

impl Derives for ValueBalanceIndexWriter {
    type Item = BlockFees;

    /// Durable prevouts = segment probes (a cold page fault each): the blocking pool's step
    async fn derive(&mut self, blocks: &[Arc<Block>]) -> Result<Vec<BlockFees>, IndexWriterError> {
        let (blocks, pending, durable) =
            (blocks.to_vec(), self.pending.clone(), self.outputs.pin());
        blocking(move || resolve(&blocks, &pending, &durable)).await
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        BlockHeader, OrchardData, ReorgDepth, SaplingData, Script, SignedZatoshis, SproutData,
        Transaction, TransparentData, TransparentOutput,
    };
    use zaino_sync::{BlockSink, FeeSink, FollowError, IndexFollower, Step};

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
                coinbase: false,
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

    fn coinbase(tag: u8, value: u64) -> Transaction {
        let mut tx = tx(tag, &[], &[value], [0; 4]);
        tx.transparent.coinbase = true;
        tx
    }

    /// Per tx fee, in zats (`None` = coinbase)
    fn fees(block_fees: &BlockFees) -> Vec<Option<u64>> {
        let paid = |fee: &Fee| match fee {
            Fee::Coinbase => None,
            Fee::Paid(fee) => Some(fee.as_u64()),
        };
        block_fees.fees.iter().map(paid).collect()
    }

    /// `A<h>[f]` / `F<h>` / `R` / `S`: a step's position in the stream, data aside
    fn label<T>(step: &Step<T>) -> String {
        match step {
            Step::Apply { height, finalized, .. } => {
                format!("A{height}{}", if *finalized { "f" } else { "" })
            }
            Step::Finalized { height } => format!("F{height}"),
            Step::Reorg => "R".to_owned(),
            Step::Shutdown => "S".to_owned(),
        }
    }

    /// Depth 2, tip 4 (0 to 2 final, both inclusive, committed one per batch; 3, 4
    /// non-finalized): every prevout resolves wherever it lives
    /// - 1: spends 0's output (durable) and one from earlier in its own block (staged)
    /// - 3: spends 1's outputs (durable) with value leaving sprout
    /// - 4: spends 3's output (non-finalized) and enters ironwood
    ///
    /// Restart with a downstream index durable at 0 (BlockSink starts at 1): 1 to 2 (both
    /// inclusive) replay through durable storage alone, 3 onward recorded again, and every item is
    /// identical
    ///
    /// Derived stream = the block stream step for step (same heights, flags, `Shutdown` last)
    ///
    /// Both batch sizes: 1 byte = one block per `deliver`; 1 MiB = the queued chain as one run
    /// (1's spend of 0's output then resolves inside the run, not from a committed segment)
    #[tokio::test]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_a_replay_republishes_them() {
        for batch in [NonZeroUsize::MIN, QUEUE] {
            resolve_every_prevout_and_replay(batch).await;
        }
    }

    async fn resolve_every_prevout_and_replay(batch: NonZeroUsize) {
        let chain = [
            block(0, 0, 0, vec![coinbase(0x10, 100_000)]),
            block(
                1,
                0,
                0,
                vec![
                    coinbase(0x11, 625_000_000),
                    tx(0x21, &[(0x10, 0)], &[60_000, 39_000], [0; 4]),
                    tx(0x22, &[(0x21, 1)], &[30_000], [0, -8_000, 0, 0]),
                ],
            ),
            block(
                2,
                0,
                0,
                vec![coinbase(0x12, 625_000_000), tx(0x23, &[(0x21, 0)], &[], [0, 0, -59_000, 0])],
            ),
            block(
                3,
                0,
                0,
                vec![
                    coinbase(0x13, 625_000_000),
                    tx(0x24, &[(0x22, 0), (0x11, 0)], &[625_029_500], [500, 0, 0, 0]),
                ],
            ),
            block(
                4,
                0,
                0,
                vec![
                    coinbase(0x14, 625_000_000),
                    tx(0x25, &[(0x24, 0)], &[625_000_000], [0, 0, 0, -29_000]),
                ],
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
        for (boot, downstream) in [("first", None), ("restart", Some(h(0)))] {
            let writer =
                ValueBalanceIndexWriter::open(fs.clone(), Path::new("/vb"), NetworkType::Regtest)
                    .expect("open");
            let durable = writer.finalized_height();
            let mut block_sink = BlockSink::new("blocks");
            let mut fee_sink = FeeSink::new("fees");
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let subscription = block_sink.subscribe("value_balance", QUEUE);
            let (_tips, tips) = watch::channel(None);
            let follower = IndexFollower::new(writer, subscription, tips, batch, depth(2))
                .publishing(fee_sink);
            let mut blocks = block_sink.subscribe("downstream", QUEUE);
            let finalized = follower.subscribe_finalized();
            let running = tokio::spawn(follower.run(CancellationToken::new()));

            // tip 4, depth 2: final through 2; from after the rearmost durable tip
            let start = durable.min(downstream).map_or(0, |tip| u32::from(tip) as usize + 1);
            for block in &chain[start..] {
                let height = block.header().height;
                let (finalized, data) = (height <= h(2), Arc::clone(block));
                block_sink.send(Step::Apply { height, finalized, data }).await;
            }
            block_sink.shutdown();
            running.await.expect("joined").expect("clean stop");

            let (mut block_steps, mut derived_steps, mut published) = (vec![], vec![], vec![]);
            loop {
                let step = blocks.next().await;
                block_steps.push(label(&step));
                if matches!(step, Step::Shutdown) {
                    break;
                }
            }
            loop {
                let step = consumer.next().await;
                derived_steps.push(label(&step));
                match step {
                    Step::Apply { data, .. } => published.push(data),
                    Step::Shutdown => break,
                    Step::Finalized { .. } | Step::Reorg => {}
                }
            }
            assert_eq!(derived_steps, block_steps, "{boot}: derived mirrors the block stream");

            match boot {
                "first" => {
                    assert_eq!(durable, None, "fresh directory");
                    let expected = ["A0f", "A1f", "A2f", "A3", "A4", "S"];
                    assert_eq!(block_steps, expected, "{boot}");
                    let published_fees: Vec<_> = published.iter().map(|b| fees(b)).collect();
                    assert_eq!(published_fees, expected_fees, "fees per tx");
                    first_boot = published;
                }
                _ => {
                    assert_eq!(durable, Some(h(2)), "0 to 2 (both inclusive) committed");
                    assert_eq!(block_steps, ["A1f", "A2f", "A3", "A4", "S"], "{boot}");
                    assert_eq!(published, first_boot[1..], "replay republishes identical fees");
                }
            }
            let finalized = *finalized.borrow();
            assert_eq!(finalized, Some(h(2)), "{boot}: final durable, non-finalized not");
        }
    }

    /// Depth 2, tip 3 on fork 0 (2, 3 non-finalized), then fork 1 wins from 2 (its 3 spends an
    /// output only its own 2 created): the derived stream carries the `Reorg` where the block
    /// stream did, then fork 1's items, resolved against fork 1's outputs
    #[tokio::test]
    async fn a_reorg_drops_the_losing_branch_outputs_and_republishes_the_winner() {
        let genesis = block(0, 0, 0, vec![coinbase(0x10, 100_000)]);
        let one = block(1, 0, 0, vec![coinbase(0x11, 625_000_000)]);
        let losing = [
            block(
                2,
                0,
                0,
                vec![coinbase(0x12, 625_000_000), tx(0x20, &[(0x10, 0)], &[99_000], [0; 4])],
            ),
            block(
                3,
                0,
                0,
                vec![coinbase(0x13, 625_000_000), tx(0x30, &[(0x20, 0)], &[98_000], [0; 4])],
            ),
        ];
        let winning = [
            block(
                2,
                1,
                0,
                vec![coinbase(0x42, 625_000_000), tx(0x60, &[(0x10, 0)], &[90_000], [0; 4])],
            ),
            block(
                3,
                1,
                1,
                vec![coinbase(0x43, 625_000_000), tx(0x70, &[(0x60, 0)], &[80_000], [0; 4])],
            ),
        ];

        let writer =
            ValueBalanceIndexWriter::open(SimFs::new(), Path::new("/vb"), NetworkType::Regtest)
                .expect("open");
        let mut block_sink = BlockSink::new("blocks");
        let mut fee_sink = FeeSink::new("fees");
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let subscription = block_sink.subscribe("value_balance", QUEUE);
        let (_tips, tips) = watch::channel(None);
        let follower = IndexFollower::new(writer, subscription, tips, NonZeroUsize::MIN, depth(2))
            .publishing(fee_sink);
        let running = tokio::spawn(follower.run(CancellationToken::new()));

        // tip 3, depth 2: 0 and 1 final, 2 and 3 not; reset → 2 and 3 again, from fork 1
        let apply = |block: &Arc<Block>| {
            let height = block.header().height;
            Step::Apply { height, finalized: height <= h(1), data: Arc::clone(block) }
        };
        for block in [&genesis, &one].into_iter().chain(&losing) {
            block_sink.send(apply(block)).await;
        }
        block_sink.send(Step::Reorg).await;
        for block in &winning {
            block_sink.send(apply(block)).await;
        }
        block_sink.shutdown();
        running.await.expect("joined").expect("clean stop");

        let mut derived = Vec::new();
        loop {
            let step = consumer.next().await;
            let seen = match &step {
                Step::Apply { data, .. } => format!("{} {:?}", label(&step), fees(data)),
                _ => label(&step),
            };
            derived.push(seen);
            if matches!(step, Step::Shutdown) {
                break;
            }
        }
        assert_eq!(
            derived,
            [
                "A0f [None]",
                "A1f [None]",
                "A2 [None, Some(1000)]",
                "A3 [None, Some(1000)]",
                "R",
                "A2 [None, Some(10000)]",
                "A3 [None, Some(10000)]",
                "S",
            ],
            "losing branch, the reset where the block stream had it, then the winner's own fees"
        );
    }

    /// Tx 0x20 of block 0 unresolvable or consensus-invalid: fatal, named; the pipeline is
    /// cancelled, and `Shutdown` still reaches the downstream consumer
    #[tokio::test]
    async fn an_unrecorded_prevout_or_a_negative_fee_stops_the_index() {
        let id = |byte| TransactionId::from([byte; 32]);
        let cases = [
            // spends an outpoint never recorded (a foreign directory, a gap)
            (
                tx(0x20, &[(0x99, 3)], &[1], [0; 4]),
                IndexWriterError::MissingPrevout {
                    height: h(0),
                    txid: id(0x20),
                    spent: id(0x99),
                    vout: 3,
                },
            ),
            // transparent outputs > inputs
            (
                tx(0x20, &[(0x10, 0)], &[100_001], [0; 4]),
                IndexWriterError::NegativeFee { height: h(0), txid: id(0x20) },
            ),
            // value into sapling from nothing
            (
                tx(0x20, &[], &[], [0, -1, 0, 0]),
                IndexWriterError::NegativeFee { height: h(0), txid: id(0x20) },
            ),
        ];

        for (invalid, expected) in cases {
            let writer =
                ValueBalanceIndexWriter::open(SimFs::new(), Path::new("/vb"), NetworkType::Regtest)
                    .expect("open");
            let mut block_sink = BlockSink::new("blocks");
            let mut fee_sink = FeeSink::new("fees");
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let subscription = block_sink.subscribe("value_balance", QUEUE);
            let (_tips, tips) = watch::channel(None);
            let follower =
                IndexFollower::new(writer, subscription, tips, NonZeroUsize::MIN, depth(2))
                    .publishing(fee_sink);
            let shutdown = CancellationToken::new();
            let running = tokio::spawn(follower.run(shutdown.clone()));

            let data = block(0, 0, 0, vec![coinbase(0x10, 100_000), invalid]);
            block_sink.send(Step::Apply { height: h(0), finalized: false, data }).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.cancelled())
                .await
                .expect("the failure cancels the pipeline");
            block_sink.shutdown();

            let stopped = running.await.expect("joined");
            assert!(
                matches!(consumer.next().await, Step::Shutdown),
                "{expected}: nothing derived for the failed block, then Shutdown"
            );
            let Err(FollowError::Index { index, source }) = &stopped else { panic!("{stopped:?}") };
            assert_eq!(*index, "value_balance");
            assert_eq!(format!("{source:?}"), format!("{expected:?}"));
        }
    }
}
