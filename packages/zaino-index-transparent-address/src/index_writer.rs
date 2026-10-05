//! transparent_address index: one block in, two projections out, kept by its own loop
//!
//! - Final block (bulk) → `bulk`, committed per batch, projected on the blocking pool beside the
//!   write
//! - Non-final block → projected into `non_finalized` (no storage touched, nothing read back)
//! - Spend recorded under its outpoint (already in the block): `outpoint → address` never resolved

use std::{collections::VecDeque, num::NonZeroUsize, path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs,
    lsm::{LsmStore, Snapshot},
    StoreError,
};
use zaino_primitives::types::{Block, BlockRef, Height, OutPoint};
use zaino_sync::{Offloaded, Published, Step, Subscription, Weight};
use zcash_protocol::consensus::NetworkType;

use crate::{
    address::address_key,
    key::{ReceiveKey, ReceiveRow, Spend, SpentRow},
    view::{NonFinalizedRows, ReadView},
    TransparentAddressIndex,
};

/// - `durable` / `receives` / `spent` = the store as of the last commit (a view never pins a
///   segment whose rows still sit in `non_finalized`)
/// - `window` = applied, not-yet-final blocks (their hashes: a commit's tip)
/// - `bulk` = final blocks not yet committed, `bulk_bytes` their [`Weight`]
pub struct TransparentAddressIndexWriter {
    segments: Offloaded<LsmStore<TransparentAddressIndex>>,
    durable: Option<BlockRef>,
    receives: Arc<Snapshot<ReceiveKey>>,
    spent: Arc<Snapshot<OutPoint>>,
    non_finalized: NonFinalizedRows,
    window: VecDeque<BlockRef>,
    bulk: Vec<Arc<Block>>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
    published: Published<ReadView>,
}

/// One block projected onto both row shapes (= everything this index derives)
fn project(block: &Block) -> (Vec<ReceiveRow>, Vec<SpentRow>) {
    let mut receives = Vec::new();
    let mut spent = Vec::new();
    let height = u32::from(block.header().height);

    for tx in block.transactions() {
        // coinbase inputs elided upstream (`zaino-source` decode.rs)
        for input in &tx.transparent.inputs {
            spent.push(SpentRow { key: *input, spend: Spend { height, spender: tx.txid } });
        }

        for (vout, output) in (0u32..).zip(&tx.transparent.outputs) {
            receives.push(ReceiveRow {
                key: ReceiveKey {
                    address: address_key(output.script.as_bytes()),
                    height,
                    txid: tx.txid,
                    vout,
                },
                value: output.value,
            });
        }
    }

    (receives, spent)
}

impl TransparentAddressIndexWriter {
    pub const NAME: &'static str = "transparent_address";

    /// Opens `path` at its committed state (every listed segment proven, every other one removed)
    ///
    /// - `batch_bytes` = final blocks per bulk commit (one fsync)
    pub fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        network: NetworkType,
        batch_bytes: NonZeroUsize,
    ) -> Result<Self, StoreError> {
        let segments = LsmStore::<TransparentAddressIndex>::open(fs, path, network)?;
        let durable = segments.committed().tip;
        let applied = durable.map(|tip| tip.height);
        let (receives, spent) = segments.sets();
        let (receives, spent) = (receives.pin(), spent.pin());
        let non_finalized = NonFinalizedRows::empty_at(applied);
        let view = ReadView {
            non_finalized: non_finalized.clone(),
            receives: Arc::clone(&receives),
            spent: Arc::clone(&spent),
        };
        Ok(Self {
            segments: Offloaded::new(segments),
            durable,
            receives,
            spent,
            non_finalized,
            window: VecDeque::new(),
            bulk: Vec::new(),
            bulk_bytes: 0,
            batch_bytes,
            published: Published::new(view, applied),
        })
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.durable
    }

    /// View, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<ReadView> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown` (a failure panics: its dropped queue fails the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized: true, data } => {
                    // replay for an index behind this one: already on disk
                    if Some(height) <= self.durable.map(|tip| tip.height) {
                        continue;
                    }
                    assert!(self.window.is_empty(), "transparent_address: final block above tip");
                    self.bulk_bytes += data.weight();
                    self.bulk.push(data);
                    self.published.merged(height);
                    if self.bulk_bytes >= self.batch_bytes.get() {
                        self.commit(height).await;
                    }
                }
                Step::Apply { height, finalized: false, data } => {
                    // bulk → tip: what bulk staged commits before the first apply builds on it
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await;
                    }
                    let next = self.non_finalized.applied().map_or(Height::GENESIS, Height::next);
                    assert_eq!(
                        height, next,
                        "transparent_address: blocks must arrive contiguously"
                    );
                    let (receives, spent) = project(&data);
                    for row in receives {
                        self.non_finalized.insert_receive(row);
                    }
                    for row in spent {
                        self.non_finalized.insert_spend(row);
                    }
                    self.non_finalized.advance(height);
                    self.window.push_back(BlockRef { hash: data.header().hash, height });
                }
                Step::Finalized { height } => self.commit(height).await,
                Step::Reorg => {
                    assert!(self.bulk.is_empty(), "transparent_address: reorg with bulk staged");
                    // back to the durable tip (segments untouched: commits final-only), the
                    // winning branch applied from there
                    self.window.clear();
                    let durable = self.durable.map(|tip| tip.height);
                    self.non_finalized = NonFinalizedRows::empty_at(durable);
                    self.publish();
                    self.published.reorged();
                }
                Step::Shutdown => {
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await;
                    }
                    return;
                }
            }
            self.publish();
        }
    }

    /// Every final block through `through` → disk: bulk blocks (projected on the blocking pool)
    /// or applied ones (rows drained from `non_finalized`), then segments pinned and the written
    /// rows dropped in one step (a row sits in exactly one tier)
    async fn commit(&mut self, through: Height) {
        let bulk = std::mem::take(&mut self.bulk);
        self.bulk_bytes = 0;
        let mut committed: Vec<BlockRef> = bulk
            .iter()
            .map(|block| BlockRef { hash: block.header().hash, height: block.header().height })
            .collect();
        while self.window.front().is_some_and(|tip| tip.height <= through) {
            committed.extend(self.window.pop_front());
        }

        let mut next = self.durable.map_or(Height::GENESIS, |tip| tip.height.next());
        for block in &committed {
            assert_eq!(block.height, next, "transparent_address: final blocks not contiguous");
            next = next.next();
        }
        let tip = *committed.last().expect("transparent_address: commit with nothing final");
        assert_eq!(tip.height, through, "transparent_address: final blocks short of {through}");

        let (mut receives, mut spent) = self.non_finalized.rows_through(Some(through));
        let written = self
            .segments
            .blocking(move |segments| {
                for block in &bulk {
                    let (block_receives, block_spent) = project(block);
                    receives.extend(block_receives);
                    spent.extend(block_spent);
                }
                // segment writes, fsyncs, the manifest and any merges
                segments.commit((receives, spent), tip)
            })
            .await;
        let segments = self.segments.get();
        if let Err(error) = written {
            error.commit_failed(Self::NAME, segments.path());
        }
        let (receives, spent) = segments.sets();
        (self.receives, self.spent) = (receives.pin(), spent.pin());
        self.durable = segments.committed().tip;
        let durable = self.durable.map(|tip| tip.height);
        self.non_finalized.land_through(durable);
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(durable);
    }

    fn publish(&self) {
        let view = ReadView {
            non_finalized: self.non_finalized.clone(),
            receives: Arc::clone(&self.receives),
            spent: Arc::clone(&self.spent),
        };
        self.published.view(view, self.non_finalized.applied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        BlockHeader, Height, OutPoint, Script, Transaction, TransactionId, TransparentData,
        TransparentOutput, Zatoshis,
    };

    use proptest::strategy::Strategy as _;
    use tokio::sync::watch;
    use zaino_persistence::fs::SimFs;
    use zaino_sync::{BlockSink, Served};
    use zcash_transparent::address::TransparentAddress;

    use crate::{key::AddressKey, TransactionRef, TransparentAddressService};

    const BATCH: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    /// Service over the view the index last published
    fn service(served: &Served<ReadView>) -> TransparentAddressService {
        let view = (*served.pin_any()).clone();
        TransparentAddressService::new(Served::fixed(view), NetworkType::Regtest)
    }

    /// Waits until `tip` publishes `at` (a last height, inclusive; `None` = none)
    async fn until(tip: &mut watch::Receiver<Option<Height>>, at: Option<Height>) {
        let within = std::time::Duration::from_secs(10);
        let reached = tokio::time::timeout(within, tip.wait_for(|now| *now == at)).await;
        reached.unwrap_or_else(|_| panic!("tip never reached {at:?}")).expect("index alive");
    }

    fn step(block: &Arc<Block>, finalized: bool) -> Step<Block> {
        Step::Apply { height: block.header().height, finalized, data: Arc::clone(block) }
    }

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn zat(n: u64) -> Zatoshis {
        Zatoshis::new(n).expect("in supply")
    }

    fn txid(tag: u8) -> TransactionId {
        TransactionId::from([tag; 32])
    }

    /// `(height, txid)` of every transaction touching `address`, genesis to `end`, both inclusive
    fn touching(
        service: &TransparentAddressService,
        address: &TransparentAddress,
        end: u32,
    ) -> Vec<(u32, TransactionId)> {
        let found = service.transactions(address, h(0), h(end)).expect("transactions");
        found.into_iter().map(|found| (found.height, found.txid)).collect()
    }

    fn p2pkh(tag: u8) -> Vec<u8> {
        [&[0x76, 0xa9, 0x14][..], &[tag; 20], &[0x88, 0xac]].concat()
    }

    fn block(height: u32, transactions: Vec<Transaction>) -> Arc<Block> {
        Arc::new(Block::new(
            BlockHeader::for_tests(
                height,
                [height as u8; 32],
                [height.wrapping_sub(1) as u8; 32],
                1_700_000_000 + height,
            ),
            transactions,
        ))
    }

    fn tx(tag: u8, inputs: Vec<(u8, u32)>, outputs: Vec<(Vec<u8>, u64)>) -> Transaction {
        Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: inputs
                    .into_iter()
                    .map(|(prev, vout)| OutPoint { txid: TransactionId::from([prev; 32]), vout })
                    .collect(),
                outputs: outputs
                    .into_iter()
                    .map(|(script, value)| TransparentOutput {
                        value: Zatoshis::new(value).expect("in supply"),
                        script: Script::new(script),
                    })
                    .collect(),
            },
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    /// Ten one-block commits (batch = 1 byte: each final `Apply` writes at once; the 9th launches
    /// a background merge), crashed at every persistence point: each state reopens to a committed
    /// prefix with its one unspent output and balance exact, only listed segments, takes the next
    /// block
    ///
    /// - block `h` pays alice `h + 1` zats (vout 0) and spends block `h - 1`'s payment
    #[tokio::test]
    async fn every_crash_state_of_commits_and_a_merge_reopens_to_a_committed_prefix() {
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let chain: Vec<Arc<Block>> = (0u32..11)
            .map(|height| {
                let spends = match height {
                    0 => Vec::new(),
                    _ => vec![(height as u8, 0)],
                };
                block(
                    height,
                    vec![tx(height as u8 + 1, spends, vec![(p2pkh(0xa1), u64::from(height) + 1)])],
                )
            })
            .collect();
        let path = Path::new("/ta");

        let fs = SimFs::recording();
        {
            let index = TransparentAddressIndexWriter::open(
                fs.clone(),
                path,
                NetworkType::Regtest,
                NonZeroUsize::MIN,
            )
            .expect("open");
            let mut durable = index.published().subscribe_finalized();
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            for (acked, block) in (1u64..).zip(&chain[..10]) {
                sink.send(step(block, true)).await;
                until(&mut durable, Some(block.header().height)).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("followed through Shutdown");
        }

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            // open removed whatever the crash left unlisted; a merge this open launched may
            // already be writing its output (and scratch), under an id above every listed one
            {
                let store = LsmStore::<TransparentAddressIndex>::open(
                    Arc::clone(&state.fs) as Arc<dyn Fs>,
                    path,
                    NetworkType::Regtest,
                )
                .unwrap_or_else(|error| panic!("{label}: {error}"));
                let (receives, spent) = store.sets();
                let listed =
                    [("receives", receives.pin().segments()), ("spent", spent.pin().segments())];
                for (set, listed) in listed {
                    let newest = listed.iter().map(|segment| segment.id).max();
                    let from_before = |name: &String| {
                        name.get(..10).and_then(|id| id.parse::<u32>().ok()) <= newest
                    };
                    let files = state.fs.list(&path.join(set)).expect("list");
                    let kept = files.iter().filter(|name| from_before(name)).count();
                    assert_eq!(kept, 2 * listed.len(), "{label}: only listed {set} + checksums");
                }
            }

            let open = |fs: Arc<SimFs>| {
                TransparentAddressIndexWriter::open(fs, path, NetworkType::Regtest, BATCH)
                    .unwrap_or_else(|error| panic!("{label}: {error}"))
            };
            let index = open(Arc::clone(&state.fs));
            // one block per commit: blocks held = commits recovered
            let count = index.durable_tip().map_or(0, |tip| u64::from(tip.height) + 1);
            let acked = [state.tag, (state.tag + 1).min(10)];
            assert!(acked.contains(&count), "{label}: recovered {count}");

            let expected = |count: u64| match count {
                0 => (Vec::new(), Zatoshis::ZERO),
                count => (vec![(count as u32 - 1, count)], zat(count)),
            };
            let observed = |service: &TransparentAddressService| {
                let utxos = service
                    .utxos(&alice, h(0))
                    .expect("utxos")
                    .into_iter()
                    .map(|utxo| (utxo.height, utxo.value.as_u64()))
                    .collect::<Vec<_>>();
                (utxos, service.balance(&alice).expect("balance"))
            };
            assert_eq!(observed(&service(&index.published().served())), expected(count), "{label}");

            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            sink.send(step(&chain[count as usize], true)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let resumed = open(Arc::clone(&state.fs));
            let after = observed(&service(&resumed.published().served()));
            assert_eq!(after, expected(count + 1), "{label}: next after recovery");
        }
    }

    #[derive(Debug, Clone)]
    enum Move {
        Apply,
        Finalize(usize),
        Reorg,
        Reopen,
    }

    /// Per block: outputs `(address 0..3, zats)`, spend picks (index into what is unspent then)
    type BlockPlan = (Vec<(u8, u64)>, Vec<usize>);

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random spend graphs through producer-legal apply / finalize / reorg / reopen step
        /// sequences: after every move, each address's utxos, balance and transactions equal a
        /// naive UTXO set's
        #[test]
        fn random_histories_answer_like_a_naive_utxo_set(
            plans in proptest::collection::vec(
                (
                    proptest::collection::vec((0u8..3, 1u64..1_000), 0..3),
                    proptest::collection::vec(0usize..8, 0..3),
                ),
                1..10,
            ),
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
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(random_history(plans, moves));
        }
    }

    /// Outpoint → (address, zats, height received, height spent)
    type Ledger = Vec<((TransactionId, u32), (u8, u64, u32, Option<(u32, TransactionId)>))>;

    async fn random_history(plans: Vec<BlockPlan>, moves: Vec<Move>) {
        let txid = |height: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&(height + 1).to_le_bytes());
            TransactionId::from(bytes)
        };
        let address = |tag: u8| TransparentAddress::PublicKeyHash([0xa0 + tag; 20]);

        // build the chain and its ledger together (spends pick from what is unspent then)
        let mut ledger: Ledger = Vec::new();
        let mut chain = Vec::new();
        for (height, (outputs, picks)) in (0u32..).zip(&plans) {
            let mut inputs = Vec::new();
            for pick in picks {
                let unspent: Vec<usize> =
                    (0..ledger.len()).filter(|&at| ledger[at].1 .3.is_none()).collect();
                if let Some(&at) = unspent.get(pick % unspent.len().max(1)) {
                    ledger[at].1 .3 = Some((height, txid(height)));
                    inputs.push(ledger[at].0);
                }
            }
            for (vout, &(tag, zats)) in (0u32..).zip(outputs) {
                ledger.push(((txid(height), vout), (tag, zats, height, None)));
            }
            chain.push(block(
                height,
                vec![Transaction {
                    txid: txid(height),
                    transparent: TransparentData {
                        coinbase: false,
                        inputs: inputs
                            .iter()
                            .map(|&(txid, vout)| OutPoint { txid, vout })
                            .collect(),
                        outputs: outputs
                            .iter()
                            .map(|&(tag, zats)| TransparentOutput {
                                value: Zatoshis::new(zats).expect("in supply"),
                                script: Script::new(p2pkh(0xa0 + tag)),
                            })
                            .collect(),
                    },
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            ));
        }

        // the model's answers once `applied` heights are indexed
        let expected = |tag: u8, applied: u32| {
            let visible = |height: u32| height < applied;
            let mut utxos = Vec::new();
            let mut touched = Vec::new();
            for &((txid, vout), (owner, zats, received, spent)) in &ledger {
                if owner != tag || !visible(received) {
                    continue;
                }
                touched.push(TransactionRef { height: received, txid });
                match spent.filter(|&(height, _)| visible(height)) {
                    Some((height, spender)) => {
                        touched.push(TransactionRef { height, txid: spender })
                    }
                    None => utxos.push((received, txid, vout, zats)),
                }
            }
            touched.sort_unstable();
            touched.dedup();
            let balance: u64 = utxos.iter().map(|utxo| utxo.3).sum();
            (utxos, balance, touched)
        };

        // batch = 1 byte: a final block writes as it arrives, so every move lands observably
        let fs = SimFs::new();
        let start = || {
            let index = TransparentAddressIndexWriter::open(
                fs.clone(),
                Path::new("/ta"),
                NetworkType::Regtest,
                NonZeroUsize::MIN,
            )
            .expect("open");
            let published = index.published();
            let tips = (published.subscribe_applied(), published.subscribe_finalized());
            let served = published.served();
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
            (sink, tokio::spawn(index.run(blocks)), tips, served)
        };
        let (mut sink, mut running, (mut applied_tip, mut durable_tip), mut served) = start();
        // blocks held from genesis: applied (every tier), finalized (durable)
        let (mut applied, mut finalized) = (0usize, 0usize);
        let tip = |count: usize| count.checked_sub(1).map(|last| h(last as u32));
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Apply if applied < chain.len() => {
                    sink.send(step(&chain[applied], false)).await;
                    applied += 1;
                }
                Move::Finalize(count) if finalized < chain.len() => {
                    // window blocks finalized oldest first, the rest (never applied) sent final
                    let end = (finalized + count).min(chain.len());
                    for (height, block) in chain.iter().enumerate().take(end).skip(finalized) {
                        match height < applied {
                            true => sink.send(Step::Finalized { height: h(height as u32) }).await,
                            false => sink.send(step(block, true)).await,
                        }
                    }
                    (finalized, applied) = (end, applied.max(end));
                }
                // an empty window leaves nothing to drop
                Move::Reorg if applied > finalized => {
                    sink.send(Step::Reorg).await;
                    applied = finalized;
                }
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("followed through Shutdown");
                    (sink, running, (applied_tip, durable_tip), served) = start();
                    applied = finalized;
                }
                _ => {}
            }
            until(&mut applied_tip, tip(applied)).await;
            until(&mut durable_tip, tip(finalized)).await;

            let applied = applied as u32;
            let service = service(&served);
            for tag in 0..3u8 {
                let (utxos, balance, touched) = expected(tag, applied);
                let served = service
                    .utxos(&address(tag), h(0))
                    .expect("utxos")
                    .into_iter()
                    .map(|utxo| (utxo.height, utxo.txid, utxo.vout, utxo.value.as_u64()))
                    .collect::<Vec<_>>();
                assert_eq!(served, utxos, "move {at} {next:?}: utxos of {tag}");
                let served = service.balance(&address(tag)).expect("balance").as_u64();
                assert_eq!(served, balance, "move {at} {next:?}: balance of {tag}");
                if applied > 0 {
                    let served = service.transactions(&address(tag), h(0), h(applied - 1));
                    let served = served.expect("transactions");
                    assert_eq!(served, touched, "move {at} {next:?}: transactions of {tag}");
                }
            }

            // every address in one request, repeats included: one batched spend lookup, same
            // answers as one address at a time
            let tags = [2u8, 0, 1, 0];
            let addresses = tags.map(address);
            let batched: Vec<Vec<_>> = service
                .utxos_of(&addresses, h(0))
                .expect("utxos of all")
                .into_iter()
                .map(|utxos| {
                    utxos
                        .into_iter()
                        .map(|utxo| (utxo.height, utxo.txid, utxo.vout, utxo.value.as_u64()))
                        .collect()
                })
                .collect();
            let one_by_one: Vec<Vec<_>> =
                tags.iter().map(|&tag| expected(tag, applied).0).collect();
            assert_eq!(batched, one_by_one, "move {at} {next:?}: utxos of all");
            let balances: Vec<u64> = service
                .balances(&addresses)
                .expect("balances of all")
                .into_iter()
                .map(|balance| balance.as_u64())
                .collect();
            let one_by_one: Vec<u64> = tags.iter().map(|&tag| expected(tag, applied).1).collect();
            assert_eq!(balances, one_by_one, "move {at} {next:?}: balances of all");
        }
        sink.shutdown();
        running.await.expect("followed through Shutdown");
    }

    /// Receive in one segment, its spend in the next, queried across both, then again after a
    /// reopen (resumes at the committed height, every earlier segment still readable)
    ///
    /// - segment 0 = bulk-sync path (final on arrival, never applied), segment 1 = the tip path
    #[tokio::test]
    async fn a_spend_in_a_later_segment_retires_a_utxo_written_by_an_earlier_one() {
        let fs = SimFs::new();
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);
        let open = || {
            let fs = fs.clone();
            TransparentAddressIndexWriter::open(fs, Path::new("/ta"), NetworkType::Regtest, BATCH)
                .expect("open")
        };
        let tips = |index: &TransparentAddressIndexWriter| {
            let published = index.published();
            (*published.subscribe_finalized().borrow(), *published.subscribe_applied().borrow())
        };

        let index = open();
        assert_eq!(tips(&index), (None, None), "an empty index starts at genesis");

        // segment 0, heights 0 and 1 sent final (bulk): alice paid twice, bob once, + one
        // opaque output; written as one batch at the Shutdown drain
        let segment_0 = [
            block(
                0,
                vec![tx(
                    0x10,
                    vec![],
                    vec![(p2pkh(0xa1), 500), (p2pkh(0xb0), 70), (vec![0x6a, 0x01], 1)],
                )],
            ),
            block(1, vec![tx(0x11, vec![], vec![(p2pkh(0xa1), 300)])]),
        ];
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        for block in &segment_0 {
            sink.send(step(block, true)).await;
        }
        sink.shutdown();
        running.await.expect("followed through Shutdown");

        let index = open();
        let two = (Some(h(1)), Some(h(1)));
        assert_eq!(tips(&index), two, "skipping the non-finalized tier carries both tips");
        let balance = service(&index.published().served()).balance(&alice).expect("balance");
        assert_eq!(balance, zat(800), "both receives unspent");

        // segment 1, height 2: alice's first output spent, paying bob; applied first → drained
        // out of the non-finalized tier by its Finalized, not re-projected
        let spend = block(2, vec![tx(0x20, vec![(0x10, 0)], vec![(p2pkh(0xb0), 490)])]);
        let served = index.published().served();
        let mut durable = index.published().subscribe_finalized();
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        sink.send(step(&spend, false)).await;
        sink.send(Step::Finalized { height: h(2) }).await;
        until(&mut durable, Some(h(2))).await;

        let service_2 = service(&served);
        let alice_balance = service_2.balance(&alice).expect("balance");
        assert_eq!(alice_balance, zat(300), "spend in segment 1 retires the receive in segment 0");
        let utxos = service_2.utxos(&alice, h(0)).expect("utxos");
        assert_eq!(utxos.len(), 1, "only the unspent receive: {utxos:?}");
        assert_eq!((utxos[0].height, utxos[0].txid, utxos[0].value), (1, txid(0x11), zat(300)));
        // paying and spending transactions both alice's, each once
        let expected = vec![(0, txid(0x10)), (1, txid(0x11)), (2, txid(0x20))];
        assert_eq!(touching(&service_2, &alice, 2), expected);
        // opaque outputs stored, not dropped (same fold answers for them)
        let opaque = service_2.balance_of(AddressKey::opaque()).expect("opaque balance");
        assert_eq!(opaque, zat(1));
        sink.shutdown();
        running.await.expect("followed through Shutdown");

        // reopen → committed height, both segments still read
        let resumed = open();
        assert_eq!(tips(&resumed), (Some(h(2)), Some(h(2))), "resumes where it committed");
        let balances = |service: &TransparentAddressService| {
            (service.balance(&alice).expect("alice"), service.balance(&bob).expect("bob"))
        };
        let resumed_balances = balances(&service(&resumed.published().served()));
        assert_eq!(resumed_balances, (zat(300), zat(560)), "segments = state, no replay");

        // third segment continues the numbering, never overwrites segment 1
        let after_restart = block(3, vec![tx(0x30, vec![(0x11, 0)], vec![(p2pkh(0xa1), 290)])]);
        let served = resumed.published().served();
        let mut durable = resumed.published().subscribe_finalized();
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(resumed.run(blocks));
        sink.send(step(&after_restart, false)).await;
        sink.send(Step::Finalized { height: h(3) }).await;
        until(&mut durable, Some(h(3))).await;
        let resumed_service = service(&served);
        let alice_balance = resumed_service.balance(&alice).expect("balance");
        assert_eq!(alice_balance, zat(290), "pre-restart receive spendable post-restart");
        let recent = resumed_service.utxos(&alice, h(3)).expect("utxos").len();
        assert_eq!(recent, 1, "start_height filters the reply, not the spend resolution");
        sink.shutdown();
        running.await.expect("followed through Shutdown");
    }

    /// Receives and spends above the committed tip answer from the non-finalized tier alone; a
    /// reorg drops all of it (segments untouched); answers then follow the re-applied branch, and
    /// committing it changes no answer, only where it is read from
    #[tokio::test]
    async fn non_finalized_answers_above_the_committed_tip_and_a_reorg_drops_only_it() {
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);
        let index = TransparentAddressIndexWriter::open(
            SimFs::new(),
            Path::new("/ta"),
            NetworkType::Regtest,
            BATCH,
        )
        .expect("open");
        let served = index.published().served();
        let (mut applied, mut durable) =
            (index.published().subscribe_applied(), index.published().subscribe_finalized());
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        let balances = |service: &TransparentAddressService| {
            (service.balance(&alice).expect("alice"), service.balance(&bob).expect("bob"))
        };

        // height 0 alone durable, everything after it non-finalized
        sink.send(step(&block(0, vec![tx(0xc0, vec![], vec![(p2pkh(0xa1), 500)])]), false)).await;
        sink.send(Step::Finalized { height: h(0) }).await;
        sink.send(step(&block(1, vec![tx(0xd1, vec![], vec![(p2pkh(0xa1), 200)])]), false)).await;
        let loser = block(2, vec![tx(0xd2, vec![(0xc0, 0)], vec![(p2pkh(0xb0), 490)])]);
        sink.send(step(&loser, false)).await;
        until(&mut applied, Some(h(2))).await;
        until(&mut durable, Some(h(0))).await;

        // committed receive, non-finalized spend: the probe crosses the boundary
        // bob: a receive nothing has committed is still an answer
        let above = service(&served);
        assert_eq!(balances(&above), (zat(200), zat(490)));
        let utxo = crate::AddressUtxo { height: 1, txid: txid(0xd1), vout: 0, value: zat(200) };
        assert_eq!(above.utxos(&alice, h(0)).expect("utxos"), vec![utxo]);
        let expected = vec![(0, txid(0xc0)), (1, txid(0xd1)), (2, txid(0xd2))];
        assert_eq!(touching(&above, &alice, 2), expected);

        // branch loses: non-finalized tier dropped whole, no segment touched, no fork height named
        sink.send(Step::Reorg).await;
        until(&mut applied, Some(h(0))).await;
        let dropped = service(&served);
        assert_eq!(*durable.borrow(), Some(h(0)));
        assert_eq!(balances(&dropped), (zat(500), zat(0)), "only durable height 0 survives");
        assert_eq!(touching(&dropped, &alice, 2), vec![(0, txid(0xc0))], "dropped: no rows");

        // winning branch = ordinary applies from the durable tip (the restart path)
        sink.send(step(&block(1, vec![tx(0xd1, vec![], vec![(p2pkh(0xa1), 200)])]), false)).await;
        let winner = block(2, vec![tx(0xe2, vec![(0xd1, 0)], vec![(p2pkh(0xb0), 190)])]);
        sink.send(step(&winner, false)).await;
        until(&mut applied, Some(h(2))).await;
        let won = service(&served);
        assert_eq!(won.balance(&alice).expect("balance"), zat(500));
        let expected = vec![(0, txid(0xc0)), (1, txid(0xd1)), (2, txid(0xe2))];
        assert_eq!(touching(&won, &alice, 2), expected, "orphan's txid gone, winner's there");

        // committing the winner changes no answer, only where it is read from
        sink.send(Step::Finalized { height: h(1) }).await;
        sink.send(Step::Finalized { height: h(2) }).await;
        until(&mut durable, Some(h(2))).await;
        let (applied_view, committed) = (balances(&won), balances(&service(&served)));
        assert_eq!(applied_view, (zat(500), zat(190)), "view pinned before the commit");
        assert_eq!(committed, applied_view, "same answers from the segments");
        sink.shutdown();
        running.await.expect("followed through Shutdown");
    }

    /// Gap = dropped receives (later spends probe unwritten rows, the address keeps a balance it no
    /// longer has) → the index panics at the gap
    #[tokio::test]
    #[should_panic(expected = "blocks must arrive contiguously")]
    async fn a_gap_in_the_block_stream_panics() {
        let index = TransparentAddressIndexWriter::open(
            SimFs::new(),
            Path::new("/ta"),
            NetworkType::Regtest,
            BATCH,
        )
        .expect("open");
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TransparentAddressIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));

        sink.send(step(&block(0, vec![tx(0xc0, vec![], vec![])]), false)).await;
        sink.send(step(&block(2, vec![tx(0xc2, vec![], vec![])]), false)).await;
        let panicked = running.await.expect_err("the gap panics the index");
        std::panic::resume_unwind(panicked.into_panic());
    }
}
