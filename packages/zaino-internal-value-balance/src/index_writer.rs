//! value_balance index: each delivered block's outputs recorded, its inputs resolved into its fees,
//! kept by its own loop
//!
//! - A run of queued blocks: outputs into `pending`, fees for the whole run resolved in one probe
//!   of durable storage
//! - Every step republished into the [`FeeSink`], 1:1 (replayed heights too: an index behind this
//!   one still pairs them)
//! - A commit moves outputs through a height from `pending` to the store's segments

use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroUsize,
    path::Path,
    sync::Arc,
};

use tokio_util::sync::CancellationToken;
use zaino_persistence::{
    fs::Fs,
    lsm::{LsmStore, SegmentSet, Snapshot},
    StoreError,
};
use zaino_primitives::types::{
    Block, BlockFees, BlockRef, Fee, Height, OutPoint, OutputIndex, Transaction, TransactionId,
    Zatoshis,
};
use zaino_sync::{
    blocking, FeeSink, IndexFailed, Offloaded, Published, Step, Subscription, Weight,
};
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
/// - `outputs` = the durable segment set (live: a pin = the segments as of the last commit)
/// - `applied` = last applied height, inclusive (`None` = none)
/// - `unwritten` = blocks whose outputs sit in `pending`, oldest first; `final_through` = the
///   highest final one (bulk), `bulk_bytes` = their [`Weight`]
/// - `batch_bytes` = one run's bytes and one bulk commit's
pub struct ValueBalanceIndexWriter {
    outputs: SegmentSet<OutPoint>,
    store: Offloaded<LsmStore<ValueBalanceIndex>>,
    durable: Option<BlockRef>,
    applied: Option<Height>,
    pending: Pending,
    unwritten: VecDeque<BlockRef>,
    final_through: Option<Height>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
    published: Published<()>,
}

impl ValueBalanceIndexWriter {
    pub const NAME: &'static str = "value_balance";

    /// Opens `path` at its committed state (every listed segment proven, every other one removed)
    ///
    /// - `batch_bytes` = final blocks per bulk write (one fsync), and one run's bytes
    pub fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        network: NetworkType,
        batch_bytes: NonZeroUsize,
    ) -> Result<Self, IndexWriterError> {
        let store = LsmStore::open(fs, path, network)?;
        let durable = store.committed().tip;
        let applied = durable.map(|tip| tip.height);
        Ok(Self {
            outputs: store.sets(),
            store: Offloaded::new(store),
            durable,
            applied,
            pending: Pending::default(),
            unwritten: VecDeque::new(),
            final_through: None,
            bulk_bytes: 0,
            batch_bytes,
            published: Published::new((), applied),
        })
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.durable
    }

    /// Tips and gate, for metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<()> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown`, republishing into `fees`; a failure cancels
    /// `cancel` (the pipeline) first, and `fees` ends with `Shutdown` either way
    pub async fn run(
        mut self,
        mut blocks: Subscription<Block>,
        fees: FeeSink,
        cancel: CancellationToken,
    ) -> Result<(), IndexFailed<IndexWriterError>> {
        let followed = self.follow(&mut blocks, &fees).await;
        if followed.is_err() {
            cancel.cancel();
        }
        blocks.skip_to_shutdown().await;
        fees.shutdown();
        followed.map_err(|source| IndexFailed { index: Self::NAME, source })
    }

    async fn follow(
        &mut self,
        blocks: &mut Subscription<Block>,
        fees: &FeeSink,
    ) -> Result<(), IndexWriterError> {
        // non-`Apply` step popped while gathering a run (handled next, keeping step order)
        let mut held: Option<Step<Block>> = None;
        loop {
            let step = match held.take() {
                Some(step) => step,
                None => blocks.next().await,
            };
            match step {
                Step::Apply { height, finalized, data } => {
                    // run = this block + every `Apply` already queued, to one batch's bytes
                    let mut bytes = data.weight();
                    let mut run = vec![(height, finalized, data)];
                    while bytes < self.batch_bytes.get() {
                        match blocks.try_next() {
                            Some(Step::Apply { height, finalized, data }) => {
                                bytes = bytes.saturating_add(data.weight());
                                run.push((height, finalized, data));
                            }
                            other => {
                                held = other;
                                break;
                            }
                        }
                    }
                    // outputs above the durable tip recorded first: a later block may spend an
                    // earlier one's (at or below = a replay for an index behind: on disk already)
                    let durable = self.durable.map(|tip| tip.height);
                    for (height, _, block) in &run {
                        if Some(*height) > durable {
                            self.pending.insert(block);
                        }
                    }
                    // the whole run's fees in one probe (cold page faults overlap)
                    let run_blocks: Vec<Arc<Block>> =
                        run.iter().map(|(_, _, block)| Arc::clone(block)).collect();
                    let (pending, outputs) = (self.pending.clone(), self.outputs.pin());
                    let run_fees =
                        blocking(move || resolve(&run_blocks, &pending, &outputs)).await?;

                    for ((height, finalized, block), block_fees) in run.into_iter().zip(run_fees) {
                        let data = Arc::new(block_fees);
                        fees.send(Step::Apply { height, finalized, data }).await;
                        let at = BlockRef { hash: block.header().hash, height };
                        if Some(height) <= durable {
                            continue;
                        }
                        if finalized {
                            assert!(
                                self.applied <= durable,
                                "value_balance: final block above non-finalized"
                            );
                            self.unwritten.push_back(at);
                            self.final_through = Some(height);
                            self.bulk_bytes += block.weight();
                            if self.bulk_bytes >= self.batch_bytes.get() {
                                self.commit(height).await?;
                            }
                        } else {
                            // bulk → tip: what bulk staged commits before the first apply
                            if let Some(through) = self.final_through {
                                self.commit(through).await?;
                            }
                            let next = self.applied.map_or(Height::GENESIS, Height::next);
                            assert_eq!(
                                height, next,
                                "value_balance: blocks must arrive contiguously"
                            );
                            self.applied = Some(height);
                            self.unwritten.push_back(at);
                        }
                    }
                }
                Step::Finalized { height } => {
                    fees.send(Step::Finalized { height }).await;
                    self.commit(height).await?;
                }
                Step::Reorg => {
                    assert!(
                        self.final_through.is_none(),
                        "value_balance: reorg with finals staged"
                    );
                    // back to the durable tip (segments untouched: commits are final-only)
                    self.pending = Pending::default();
                    self.unwritten.clear();
                    self.applied = self.durable.map(|tip| tip.height);
                    self.publish();
                    self.published.reorged();
                    fees.send(Step::Reorg).await;
                }
                Step::Shutdown => {
                    if let Some(through) = self.final_through {
                        self.commit(through).await?;
                    }
                    return Ok(());
                }
            }
            self.publish();
        }
    }

    /// Outputs of every block through `through` → disk, then they leave `pending` (segments answer
    /// for them)
    async fn commit(&mut self, through: Height) -> Result<(), IndexWriterError> {
        let mut next = self.durable.map_or(Height::GENESIS, |tip| tip.height.next());
        let mut tip = None;
        while self.unwritten.front().is_some_and(|block| block.height <= through) {
            let block = self.unwritten.pop_front().expect("front checked");
            assert_eq!(
                block.height, next,
                "value_balance: final blocks not contiguous from durable"
            );
            next = next.next();
            tip = Some(block);
        }
        let tip =
            tip.unwrap_or_else(|| panic!("value_balance: nothing to commit through {through}"));
        assert_eq!(tip.height, through, "value_balance: final blocks short of {through}");

        let rows = self.pending.rows_through(Some(through));
        let written: Vec<OutPoint> = rows.iter().map(|row| row.key).collect();
        self.store.blocking(move |store| store.commit(rows, tip)).await?;

        self.durable = self.store.get().committed().tip;
        self.pending.remove(&written);
        if self.final_through <= Some(through) {
            self.final_through = None;
            self.bulk_bytes = 0;
        }
        let durable = self.durable.map(|tip| tip.height);
        self.applied = self.applied.max(durable);
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(durable);
        Ok(())
    }

    fn publish(&self) {
        self.published.view((), self.applied);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        BlockHeader, OrchardData, SaplingData, Script, SignedZatoshis, SproutData, Transaction,
        TransparentData, TransparentOutput,
    };
    use zaino_sync::BlockSink;

    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
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

    /// Four final blocks sent one at a time (a 1-byte batch = one commit each), crashed after every
    /// operation: each state reopens to an acknowledged or attempted commit, and the index run on
    /// it resolves the next block's fees against the outputs it recovered and commits it
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_whose_outputs_still_resolve() {
        let path = Path::new("/vb");
        // block h spends an output of block h - 1 (and 3 spends one of 1): each fee needs a
        // recovered output
        let chain = [
            block(0, 0, 0, vec![coinbase(0x10, 100_000)]),
            block(1, 0, 0, vec![coinbase(0x11, 50_000), tx(0x20, &[(0x10, 0)], &[90_000], [0; 4])]),
            block(2, 0, 0, vec![coinbase(0x12, 50_000), tx(0x21, &[(0x20, 0)], &[80_000], [0; 4])]),
            block(3, 0, 0, vec![coinbase(0x13, 50_000), tx(0x22, &[(0x11, 0)], &[40_000], [0; 4])]),
            block(4, 0, 0, vec![coinbase(0x14, 50_000), tx(0x23, &[(0x21, 0)], &[70_000], [0; 4])]),
        ];
        let paid = vec![None, Some(10_000)];
        let expected = [vec![None], paid.clone(), paid.clone(), paid.clone(), paid];
        let within = Duration::from_secs(5);

        // commits of 0..=3 (4 only ever committed after a recovery); tag = commits acknowledged
        let fs = SimFs::recording();
        let index = ValueBalanceIndexWriter::open(
            fs.clone(),
            path,
            NetworkType::Regtest,
            NonZeroUsize::MIN,
        )
        .expect("open");
        let mut durable = index.published().subscribe_finalized();
        let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let blocks = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(blocks, fee_sink, CancellationToken::new()));
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            let height = block.header().height;
            block_sink.send(Step::Apply { height, finalized: true, data: Arc::clone(block) }).await;
            let landed = tokio::time::timeout(within, durable.wait_for(|at| *at == Some(height)));
            landed.await.expect("each block commits").expect("index alive");
            fs.set_tag(acked);
        }
        block_sink.shutdown();
        running.await.expect("joined").expect("clean stop");
        consumer.skip_to_shutdown().await;
        let tip_after = |commits: u64| (commits > 0).then(|| h((commits - 1).min(3) as u32));

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let crashed = &state.label;
            let index = ValueBalanceIndexWriter::open(state.fs, path, NetworkType::Regtest, QUEUE)
                .unwrap_or_else(|error| panic!("{crashed}: {error}"));
            let tip = index.durable_tip().map(|tip| tip.height);
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{crashed}: recovered through {tip:?}");

            let next = tip.map_or(0, |tip| u32::from(tip) + 1);
            let durable = index.published().subscribe_finalized();
            let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let blocks = block_sink.subscribe("value_balance", QUEUE);
            let running = tokio::spawn(index.run(blocks, fee_sink, CancellationToken::new()));
            let data = Arc::clone(&chain[next as usize]);
            block_sink.send(Step::Apply { height: h(next), finalized: true, data }).await;
            block_sink.shutdown();
            let stopped = running.await.expect("joined");
            stopped.unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));

            let resolved = match consumer.next().await {
                Step::Apply { data, .. } => fees(&data),
                other => panic!("{crashed}: fees of block {next} expected, got {}", label(&other)),
            };
            assert_eq!(resolved, expected[next as usize], "{crashed}: fees of block {next}");
            assert!(
                matches!(consumer.next().await, Step::Shutdown),
                "{crashed}: one block, then stop"
            );
            assert_eq!(*durable.borrow(), Some(h(next)), "{crashed}: written, landed");
        }
    }

    /// Tip 4 (0 to 2 final, both inclusive; 3, 4 non-finalized): every prevout resolves wherever
    /// it lives
    /// - 1: spends 0's output (durable) and one from earlier in its own block (staged)
    /// - 3: spends 1's outputs (durable) with value leaving sprout
    /// - 4: spends 3's output (non-finalized) and enters ironwood
    ///
    /// Restart with a downstream index durable at 0 (BlockSink starts at 1): 1 to 2 (both
    /// inclusive) replay through durable storage alone, 3 onward recorded again, and every item is
    /// identical; the fee stream = the block stream step for step (`Shutdown` last)
    ///
    /// Both batch sizes: 1 byte = one block per run; 1 MiB = the queued chain as one run (1's
    /// spend of 0's output then resolves inside the run, not from a committed segment)
    #[tokio::test]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_a_replay_republishes_them() {
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

        for batch in [NonZeroUsize::MIN, QUEUE] {
            let fs = SimFs::new();
            let mut first_boot = Vec::new();
            for (boot, downstream) in [("first", None), ("restart", Some(h(0)))] {
                let index = ValueBalanceIndexWriter::open(
                    fs.clone(),
                    Path::new("/vb"),
                    NetworkType::Regtest,
                    batch,
                )
                .expect("open");
                let durable = index.durable_tip().map(|tip| tip.height);
                let finalized = index.published().subscribe_finalized();
                let mut block_sink = BlockSink::new("blocks");
                let mut fee_sink = FeeSink::new("fees");
                let mut consumer = fee_sink.subscribe("consumer", QUEUE);
                let subscription = block_sink.subscribe("value_balance", QUEUE);
                let mut blocks = block_sink.subscribe("downstream", QUEUE);
                let running =
                    tokio::spawn(index.run(subscription, fee_sink, CancellationToken::new()));

                // tip 4, depth 2: final through 2; from after the rearmost durable tip
                let start = durable.min(downstream).map_or(0, |tip| u32::from(tip) as usize + 1);
                for block in &chain[start..] {
                    let height = block.header().height;
                    let (finalized, data) = (height <= h(2), Arc::clone(block));
                    block_sink.send(Step::Apply { height, finalized, data }).await;
                }
                block_sink.shutdown();
                running.await.expect("joined").expect("clean stop");

                let (mut block_steps, mut fee_steps, mut published) = (vec![], vec![], vec![]);
                loop {
                    let step = blocks.next().await;
                    block_steps.push(label(&step));
                    if matches!(step, Step::Shutdown) {
                        break;
                    }
                }
                loop {
                    let step = consumer.next().await;
                    fee_steps.push(label(&step));
                    match step {
                        Step::Apply { data, .. } => published.push(data),
                        Step::Shutdown => break,
                        Step::Finalized { .. } | Step::Reorg => {}
                    }
                }
                let context = format!("{boot}, batch {batch}");
                assert_eq!(fee_steps, block_steps, "{context}: fees mirror the block stream");
                assert_eq!(*finalized.borrow(), Some(h(2)), "{context}: final durable, rest not");

                match boot {
                    "first" => {
                        assert_eq!(durable, None, "fresh directory");
                        assert_eq!(
                            block_steps,
                            ["A0f", "A1f", "A2f", "A3", "A4", "S"],
                            "{context}"
                        );
                        let published_fees: Vec<_> = published.iter().map(|b| fees(b)).collect();
                        assert_eq!(published_fees, expected_fees, "{context}: fees per tx");
                        first_boot = published;
                    }
                    _ => {
                        assert_eq!(durable, Some(h(2)), "0 to 2 (both inclusive) committed");
                        assert_eq!(block_steps, ["A1f", "A2f", "A3", "A4", "S"], "{context}");
                        assert_eq!(
                            published,
                            first_boot[1..],
                            "{context}: replay = identical fees"
                        );
                    }
                }
            }
        }
    }

    /// Tip 3 on fork 0 (2, 3 non-finalized), then fork 1 wins from 2 (its 3 spends an output only
    /// its own 2 created): the fee stream carries the `Reorg` where the block stream did, then
    /// fork 1's fees, resolved against fork 1's outputs
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

        let fs = SimFs::new();
        let index = ValueBalanceIndexWriter::open(
            fs,
            Path::new("/vb"),
            NetworkType::Regtest,
            NonZeroUsize::MIN,
        )
        .expect("open");
        let mut block_sink = BlockSink::new("blocks");
        let mut fee_sink = FeeSink::new("fees");
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let subscription = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(subscription, fee_sink, CancellationToken::new()));

        // tip 3, depth 2: 0 and 1 final, 2 and 3 not; reorg → 2 and 3 again, from fork 1
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

        let mut published = Vec::new();
        loop {
            let step = consumer.next().await;
            let seen = match &step {
                Step::Apply { data, .. } => format!("{} {:?}", label(&step), fees(data)),
                _ => label(&step),
            };
            published.push(seen);
            if matches!(step, Step::Shutdown) {
                break;
            }
        }
        assert_eq!(
            published,
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
            "losing branch, the reorg where the block stream had it, then the winner's own fees"
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
            let fs = SimFs::new();
            let index = ValueBalanceIndexWriter::open(
                fs,
                Path::new("/vb"),
                NetworkType::Regtest,
                NonZeroUsize::MIN,
            )
            .expect("open");
            let mut block_sink = BlockSink::new("blocks");
            let mut fee_sink = FeeSink::new("fees");
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let subscription = block_sink.subscribe("value_balance", QUEUE);
            let cancel = CancellationToken::new();
            let running = tokio::spawn(index.run(subscription, fee_sink, cancel.clone()));

            let data = block(0, 0, 0, vec![coinbase(0x10, 100_000), invalid]);
            block_sink.send(Step::Apply { height: h(0), finalized: false, data }).await;
            tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
                .await
                .expect("the failure cancels the pipeline");
            block_sink.shutdown();

            let stopped = running.await.expect("joined");
            assert!(
                matches!(consumer.next().await, Step::Shutdown),
                "{expected}: no fees for the failed block, then Shutdown"
            );
            let Err(IndexFailed { index, source }) = &stopped else { panic!("{stopped:?}") };
            assert_eq!(*index, "value_balance");
            assert_eq!(format!("{source:?}"), format!("{expected:?}"));
        }
    }
}
