//! value_balance index: each delivered block's outputs recorded, its inputs resolved into its fees,
//! kept by its own loop
//!
//! - A run of queued blocks: each one's outputs held (staged or applied), then the whole run's
//!   fees resolved in one probe of every tier
//! - Every step republished into the [`FeeSink`], 1:1 (replayed heights too: an index behind this
//!   one still pairs them)
//! - one block = its outputs' rows; storage tiers = `zaino_persistence::Tiered`

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use zaino_persistence::{Changes, MapRead, Store, Tiered, TieredView};
use zaino_primitives::types::{
    Block, BlockFees, BlockRef, Fee, Height, OutPoint, OutputIndex, Transaction, TransactionId,
    Zatoshis,
};
use zaino_sync::{blocking, Applied, FeeSink, Offloaded, Published, Step, Subscription, Weight};

use crate::{decode_value, encode_value, OUTPUTS, VALUE};

#[derive(Debug, thiserror::Error)]
pub enum IndexWriterError {
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
/// - `batch_bytes` = one run's bytes and one bulk commit's
pub struct ValueBalanceIndexWriter<S: Store> {
    tiered: Offloaded<Tiered<S>>,
    batch_bytes: NonZeroUsize,
    published: Published<()>,
}

const NAME: &str = zaino_persistence::IndexKind::ValueBalance.name();

impl<S: Store<View: MapRead>> ValueBalanceIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = final blocks per
    /// bulk commit (one fsync), and one run's bytes
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let tiered = Tiered::new(store, batch_bytes);
        let published = Published::new((), tiered.durable_tip());
        Self { tiered: Offloaded::new(tiered), batch_bytes, published }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.tiered.get().durable_tip()
    }

    /// Tips and gate, for metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<()> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown`, republished into `fees`
    ///
    /// - a failure panics (dropped `fees` = no `Shutdown`: compact-block panics too)
    pub async fn run(mut self, mut blocks: Subscription<Block>, fees: FeeSink) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized, data } => {
                    let run = blocks.run((height, finalized, data), self.batch_bytes);
                    self.apply_run(run, &fees).await
                }
                Step::Finalized { height } => {
                    fees.send(Step::Finalized { height }).await;
                    self.finalize(height).await
                }
                Step::Reorg => self.reorg(&fees).await,
                Step::Shutdown => return self.shutdown(fees).await,
            }
        }
    }

    /// Each block above `replayed_through` held (a later block may spend an earlier one's), then
    /// every block's fees out (replays too), the whole run resolved in one probe (cold page
    /// faults overlap)
    async fn apply_run(&mut self, run: Vec<Applied<Block>>, fees: &FeeSink) {
        // at or below = replay for an index behind this one: on disk already (taken before the
        // run: a commit inside it moves the durable tip)
        let replayed_through = self.durable_tip().map(|tip| tip.height);
        for (height, finalized, block) in &run {
            if Some(*height) > replayed_through {
                self.hold(*height, *finalized, block).await;
            }
        }
        let blocks: Vec<Arc<Block>> = run.iter().map(|(_, _, block)| Arc::clone(block)).collect();
        let view = self.tiered.get().view();
        let run_fees = blocking(move || resolve(&blocks, &view))
            .await
            .unwrap_or_else(|error| panic!("{NAME} index: {error}"));
        for ((height, finalized, _), block_fees) in run.into_iter().zip(run_fees) {
            fees.send(Step::Apply { height, finalized, data: Arc::new(block_fees) }).await;
        }
    }

    /// Final block (bulk sync) staged for the next batch commit; a tip block applied above
    /// durable (staged blocks written first)
    async fn hold(&mut self, height: Height, finalized: bool, block: &Block) {
        let changes = self.changes(block);
        if finalized {
            let full = self.tiered.get_mut().stage(changes, block.weight());
            self.published.merged(height);
            if full {
                self.finalize(height).await;
            }
            return;
        }
        self.finalize_staged().await;
        self.tiered.get_mut().apply(changes);
        self.publish();
    }

    /// Back to the durable tip (store untouched: commits final-only)
    async fn reorg(&mut self, fees: &FeeSink) {
        self.tiered.get_mut().reorg();
        self.publish();
        self.published.reorged();
        fees.send(Step::Reorg).await;
    }

    async fn shutdown(mut self, fees: FeeSink) {
        self.finalize_staged().await;
        fees.shutdown();
    }

    /// Staged bulk → disk (before a tip block builds on it, and at `Shutdown`)
    async fn finalize_staged(&mut self) {
        if let Some(staged) = self.tiered.get().staged() {
            self.finalize(staged.height).await;
        }
    }

    /// Every held block through `through` → disk
    async fn finalize(&mut self, through: Height) {
        self.tiered.blocking(move |tiered| tiered.finalize(through)).await;
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(self.durable_tip().map(|tip| tip.height));
    }

    /// `block`'s outputs, one row each
    fn changes(&self, block: &Block) -> Changes {
        let header = block.header();
        let tip = BlockRef { hash: header.hash, height: header.height };
        let mut changes = Changes::new(tip, self.tiered.get().schema());
        for tx in block.transactions() {
            for (vout, output) in (0..).zip(&tx.transparent.outputs) {
                let key = OutPoint { txid: tx.txid, vout }.encode();
                changes.insert(OUTPUTS, &key, &encode_value(output.value));
            }
        }
        changes
    }

    fn publish(&self) {
        self.published.view((), self.tiered.get().applied());
    }
}

/// Each of `blocks`' fees, every prevout found in `view` (the whole run's outputs held)
///
/// - the whole run's prevouts in one `values` call (cold page faults overlap instead of queueing
///   one block behind another)
/// - later blocks' outputs held too: harmless (no block spends an output created after it)
fn resolve<V: MapRead>(
    blocks: &[Arc<Block>],
    view: &TieredView<V>,
) -> Result<Vec<BlockFees>, IndexWriterError> {
    let prevouts: Vec<OutPoint> = blocks
        .iter()
        .flat_map(|block| block.transactions())
        .flat_map(|tx| tx.transparent.inputs.iter().copied())
        .collect();
    let keys: Vec<[u8; OutPoint::LEN]> = prevouts.iter().map(OutPoint::encode).collect();
    let keys: Vec<&[u8]> = keys.iter().map(|key| &key[..]).collect();
    let mut values: HashMap<OutPoint, Zatoshis> = HashMap::new();
    for (key, found) in prevouts.iter().zip(view.values(OUTPUTS, &keys)) {
        let Some(found) = found else { continue };
        let bytes: &[u8; VALUE] = found[..].try_into().expect("outputs values: schema width");
        let value = decode_value(bytes).expect("outputs values: in supply when committed");
        values.insert(*key, value);
    }

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
    use std::{path::Path, time::Duration};

    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, PersistenceEngine};
    use zaino_primitives::testing::{linked, Chain};
    use zaino_primitives::types::{
        OrchardData, SaplingData, Script, SignedZatoshis, SproutData, Transaction, TransparentData,
        TransparentOutput,
    };
    use zaino_sync::BlockSink;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::schema;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// `/vb` on `fs` at its committed state
    fn open(fs: Arc<SimFs>, batch: NonZeroUsize) -> ValueBalanceIndexWriter<DiskStore> {
        let store = DiskEngine::new(fs).open(Path::new("/vb"), &schema(NetworkType::Regtest));
        ValueBalanceIndexWriter::new(store.expect("open"), batch)
    }

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

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
        // block h spends an output of block h - 1 (and 3 spends one of 1): each fee needs a
        // recovered output
        let chain = linked(vec![
            vec![coinbase(0x10, 100_000)],
            vec![coinbase(0x11, 50_000), tx(0x20, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x12, 50_000), tx(0x21, &[(0x20, 0)], &[80_000], [0; 4])],
            vec![coinbase(0x13, 50_000), tx(0x22, &[(0x11, 0)], &[40_000], [0; 4])],
            vec![coinbase(0x14, 50_000), tx(0x23, &[(0x21, 0)], &[70_000], [0; 4])],
        ]);
        let paid = vec![None, Some(10_000)];
        let expected = [vec![None], paid.clone(), paid.clone(), paid.clone(), paid];
        let within = Duration::from_secs(5);

        // commits of 0..=3 (4 only ever committed after a recovery); tag = commits acknowledged
        let fs = SimFs::recording();
        let index = open(fs.clone(), NonZeroUsize::MIN);
        let mut durable = index.published().subscribe_finalized();
        let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let blocks = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(blocks, fee_sink));
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            let height = block.header().height;
            block_sink.send(Step::Apply { height, finalized: true, data: Arc::clone(block) }).await;
            let landed = tokio::time::timeout(within, durable.wait_for(|at| *at == Some(height)));
            landed.await.expect("each block commits").expect("index alive");
            fs.set_tag(acked);
        }
        block_sink.shutdown();
        running.await.expect("clean stop");
        for (height, expected) in (0u32..).zip(&expected[..4]) {
            let Step::Apply { data, .. } = consumer.next().await else {
                panic!("fees of block {height} expected");
            };
            assert_eq!(fees(&data), *expected, "fees of block {height}");
        }
        assert!(matches!(consumer.next().await, Step::Shutdown), "Shutdown after block 3's fees");
        let tip_after = |commits: u64| (commits > 0).then(|| h((commits - 1).min(3) as u32));

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let crashed = &state.label;
            let index = open(state.fs, QUEUE);
            let tip = index.durable_tip().map(|tip| tip.height);
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{crashed}: recovered through {tip:?}");

            let next = tip.map_or(0, |tip| u32::from(tip) + 1);
            let durable = index.published().subscribe_finalized();
            let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let blocks = block_sink.subscribe("value_balance", QUEUE);
            let running = tokio::spawn(index.run(blocks, fee_sink));
            let data = Arc::clone(&chain[next as usize]);
            block_sink.send(Step::Apply { height: h(next), finalized: true, data }).await;
            block_sink.shutdown();
            running
                .await
                .unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));

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

    /// Bulk sync whose commit lands inside a run: 0 alone (its bytes carried, under the batch, its
    /// height published as merged),
    /// then 1 to 3 queued as one run whose batch fills at 2. 3 is still final after that commit,
    /// staged and committed at `Shutdown`, and every block's fees go out in order
    #[tokio::test]
    async fn a_commit_inside_a_run_keeps_the_rest_of_the_run_final() {
        let chain = linked(vec![
            vec![coinbase(0x10, 100_000)],
            vec![coinbase(0x11, 50_000), tx(0x20, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x12, 50_000), tx(0x21, &[(0x20, 0)], &[80_000], [0; 4])],
            vec![coinbase(0x13, 50_000), tx(0x22, &[(0x21, 0)], &[70_000], [0; 4])],
        ]);
        let batch = chain[..3].iter().map(|block| block.weight()).sum::<usize>();
        let index = open(SimFs::new(), NonZeroUsize::new(batch).expect("blocks weigh something"));
        let durable = index.published().subscribe_finalized();
        let mut merged = index.published().subscribe_merged();
        let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let blocks = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(blocks, fee_sink));

        let first = Arc::clone(&chain[0]);
        block_sink.send(Step::Apply { height: h(0), finalized: true, data: first }).await;
        assert_eq!(label(&consumer.next().await), "A0f", "0 handled as a run of its own");
        let held = merged.wait_for(|at| *at == Some(h(0)));
        let held = tokio::time::timeout(Duration::from_secs(5), held).await;
        held.expect("0 merged for the next bulk commit").expect("index alive");
        assert_eq!(*durable.borrow(), None, "0's bytes under the batch: carried, not committed");
        for block in &chain[1..] {
            let (height, data) = (block.header().height, Arc::clone(block));
            block_sink.send(Step::Apply { height, finalized: true, data }).await;
        }
        block_sink.shutdown();
        running.await.expect("clean stop");

        let mut steps = Vec::new();
        loop {
            let step = consumer.next().await;
            steps.push(label(&step));
            if matches!(step, Step::Shutdown) {
                break;
            }
        }
        assert_eq!(steps, ["A1f", "A2f", "A3f", "S"]);
        assert_eq!(*durable.borrow(), Some(h(3)), "2 committed mid-run, 3 at shutdown");
        assert_eq!(*merged.borrow(), None, "nothing held once a commit covers it");
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
    #[rustfmt::skip]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_a_replay_republishes_them() {
        //      tx(tag,  spends (tag, vout),      outputs,           shielded balances
        let chain = linked(vec![
            vec![
                coinbase(0x10, 100_000),
            ],
            vec![
                coinbase(0x11, 625_000_000),
                tx(0x21, &[(0x10, 0)],            &[60_000, 39_000], [0; 4]),
                tx(0x22, &[(0x21, 1)],            &[30_000],         [0, -8_000, 0, 0]),
            ],
            vec![
                coinbase(0x12, 625_000_000),
                tx(0x23, &[(0x21, 0)],            &[],               [0, 0, -59_000, 0]),
            ],
            vec![
                coinbase(0x13, 625_000_000),
                tx(0x24, &[(0x22, 0), (0x11, 0)], &[625_029_500],    [500, 0, 0, 0]),
            ],
            vec![
                coinbase(0x14, 625_000_000),
                tx(0x25, &[(0x24, 0)],            &[625_000_000],    [0, 0, 0, -29_000]),
            ],
        ]);
        // per block, per tx (None = coinbase)
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
                let index = open(fs.clone(), batch);
                let durable = index.durable_tip().map(|tip| tip.height);
                let finalized = index.published().subscribe_finalized();
                let mut block_sink = BlockSink::new("blocks");
                let mut fee_sink = FeeSink::new("fees");
                let mut consumer = fee_sink.subscribe("consumer", QUEUE);
                let subscription = block_sink.subscribe("value_balance", QUEUE);
                let mut blocks = block_sink.subscribe("downstream", QUEUE);
                let running = tokio::spawn(index.run(subscription, fee_sink));

                // tip 4, depth 2: final through 2; from after the rearmost durable tip
                let start = durable.min(downstream).map_or(0, |tip| u32::from(tip) as usize + 1);
                for block in &chain[start..] {
                    let height = block.header().height;
                    let (finalized, data) = (height <= h(2), Arc::clone(block));
                    block_sink.send(Step::Apply { height, finalized, data }).await;
                }
                block_sink.shutdown();
                running.await.expect("clean stop");

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
                        let steps = ["A0f", "A1f", "A2f", "A3", "A4", "S"];
                        assert_eq!(block_steps, steps, "{context}");
                        let published_fees: Vec<_> = published.iter().map(|b| fees(b)).collect();
                        assert_eq!(published_fees, expected_fees, "{context}: fees per tx");
                        first_boot = published;
                    }
                    _ => {
                        assert_eq!(durable, Some(h(2)), "0 to 2 (both inclusive) committed");
                        assert_eq!(block_steps, ["A1f", "A2f", "A3", "A4", "S"], "{context}");
                        assert_eq!(published, first_boot[1..], "{context}: replay = same fees");
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
        let mut chain = Chain::with_genesis(vec![coinbase(0x10, 100_000)]);
        let one = chain.mine_with(chain.genesis().hash, vec![coinbase(0x11, 625_000_000)]);
        // both branches fork from `one`: each mines `(2, 3)` from its own transactions
        let mut branch = |txs: [Vec<Transaction>; 2]| {
            let [two, three] = txs;
            let two = chain.mine_with(one.hash, two);
            let three = chain.mine_with(two.hash, three);
            chain.path(three.hash).into_iter().map(Arc::new).collect::<Vec<_>>()
        };
        let losing = branch([
            vec![coinbase(0x12, 625_000_000), tx(0x20, &[(0x10, 0)], &[99_000], [0; 4])],
            vec![coinbase(0x13, 625_000_000), tx(0x30, &[(0x20, 0)], &[98_000], [0; 4])],
        ]);
        let winning = branch([
            vec![coinbase(0x42, 625_000_000), tx(0x60, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x43, 625_000_000), tx(0x70, &[(0x60, 0)], &[80_000], [0; 4])],
        ]);

        let fs = SimFs::new();
        let index = open(fs, NonZeroUsize::MIN);
        let mut block_sink = BlockSink::new("blocks");
        let mut fee_sink = FeeSink::new("fees");
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let subscription = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(subscription, fee_sink));

        // tip 3, depth 2: 0 and 1 final, 2 and 3 not; reorg → 2 and 3 again, from fork 1
        let apply = |block: &Arc<Block>| {
            let height = block.header().height;
            Step::Apply { height, finalized: height <= h(1), data: Arc::clone(block) }
        };
        for block in &losing {
            block_sink.send(apply(block)).await;
        }
        block_sink.send(Step::Reorg).await;
        for block in &winning[2..] {
            block_sink.send(apply(block)).await;
        }
        block_sink.shutdown();
        running.await.expect("clean stop");

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

    /// Tx 0x20 of block 0 unresolvable or consensus-invalid: the index panics, named; the
    /// downstream consumer never sees `Shutdown` (its sink dropped → it panics too)
    #[tokio::test]
    async fn an_unrecorded_prevout_or_a_negative_fee_panics_the_index_and_its_consumer() {
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
            let index = open(fs, NonZeroUsize::MIN);
            let mut block_sink = BlockSink::new("blocks");
            let mut fee_sink = FeeSink::new("fees");
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let subscription = block_sink.subscribe("value_balance", QUEUE);
            let running = tokio::spawn(index.run(subscription, fee_sink));
            let downstream = tokio::spawn(async move { consumer.next().await });

            let data = Arc::clone(&linked(vec![vec![coinbase(0x10, 100_000), invalid]])[0]);
            block_sink.send(Step::Apply { height: h(0), finalized: false, data }).await;

            let message = |joined: Result<_, tokio::task::JoinError>| {
                let payload = joined.expect_err("panicked").into_panic();
                payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
            };
            let index = message(running.await);
            assert_eq!(index, Some(format!("value_balance index: {expected}")));
            let consumer = message(downstream.await.map(drop));
            assert_eq!(consumer.as_deref(), Some("sink dropped without Shutdown"), "{expected}");
        }
    }
}
