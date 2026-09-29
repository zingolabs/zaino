//! [`IndexWriter`]: one block in, two projections out, one segment each per `finalize`
//!
//! - `apply` touches no storage, reads nothing back
//! - spend recorded under its outpoint (already in the block): `outpoint → address` never resolved
//! - `finalize` re-derives what never reached the nonfinalised tier (bulk sync skips `apply`)

use std::{path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs,
    lsm::{LsmStore, Snapshot},
    manifest::Committed,
    StoreError,
};
use zaino_primitives::types::{Block, BlockHash, Extent, OutPoint};
use zaino_sync::{IndexWriter, Offloaded};
use zcash_protocol::consensus::NetworkType;

use crate::{
    address::address_key,
    key::{ReceiveKey, ReceiveRow, Spend, SpentRow},
    view::{NonFinalizedRows, ReadView},
    TransparentAddressIndex,
};

/// - `durable` = the store as of the last landing (answered without the store while a write has
///   it; a view never pins a segment whose rows still sit in the nonfinalised tier)
pub struct TransparentAddressIndexWriter {
    segments: Offloaded<LsmStore<TransparentAddressIndex>>,
    durable: Durable,
    non_finalized: NonFinalizedRows,
}

/// What the store committed, pinned at a landing
struct Durable {
    committed: Committed,
    receives: Arc<Snapshot<ReceiveKey>>,
    spent: Arc<Snapshot<OutPoint>>,
}

impl Durable {
    fn of(segments: &LsmStore<TransparentAddressIndex>) -> Self {
        let (receives, spent) = segments.sets();
        Self { committed: segments.committed(), receives: receives.pin(), spent: spent.pin() }
    }
}

/// A finished `finalize` write: the store back
pub struct Landing {
    segments: LsmStore<TransparentAddressIndex>,
}

impl TransparentAddressIndexWriter {
    /// Opens `path` at its committed state (every listed segment proven, every other one removed)
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self, StoreError> {
        let segments = LsmStore::<TransparentAddressIndex>::open(fs, path, network)?;
        let non_finalized = NonFinalizedRows::empty_at(segments.committed().extent);
        Ok(Self {
            durable: Durable::of(&segments),
            segments: Offloaded::new(segments),
            non_finalized,
        })
    }
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

impl IndexWriter for TransparentAddressIndexWriter {
    type Input = Block;
    type View = ReadView;
    type Error = StoreError;
    type Done = Landing;

    const NAME: &'static str = "transparent_address";

    fn finalized_height(&self) -> Extent {
        self.durable.committed.extent
    }

    fn finalized_tip(&self) -> Option<BlockHash> {
        self.durable.committed.tip
    }

    fn applied_height(&self) -> Extent {
        self.non_finalized.applied()
    }

    /// Rows + both segment sets as of the last landing (segments pinned and rows drained in one
    /// writer step: a row sits in exactly one tier)
    fn view(&self) -> ReadView {
        ReadView {
            non_finalized: self.non_finalized.clone(),
            receives: Arc::clone(&self.durable.receives),
            spent: Arc::clone(&self.durable.spent),
        }
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), StoreError> {
        let (height, expected) = (block.header().height, self.non_finalized.applied().next());
        assert_eq!(height, expected, "transparent_address: blocks must arrive contiguously");

        let (receives, spent) = project(block);
        for row in receives {
            self.non_finalized.insert_receive(row);
        }
        for row in spent {
            self.non_finalized.insert_spend(row);
        }
        self.non_finalized.advance(height);

        Ok(())
    }

    /// Blocks never applied (bulk) are projected by the write, off the follower
    async fn finalize(
        &mut self,
        blocks: &[Arc<Block>],
    ) -> Result<impl FnOnce() -> Result<Landing, StoreError> + Send + 'static, StoreError> {
        assert!(!blocks.is_empty(), "transparent_address: finalize with no blocks");
        let mut reached = self.finalized_height();
        for block in blocks {
            let height = block.header().height;
            assert_eq!(height, reached.next(), "transparent_address: batch off the committed end");
            reached = Extent::through(height);
        }
        let tip = blocks.last().expect("finalize asserted non-empty").header().hash;

        let applied = self.non_finalized.applied();
        let (mut receives, mut spent) = self.non_finalized.rows_below(reached);
        // above the nonfinalised extent = never through `apply` (bulk sync)
        let unapplied: Vec<Arc<Block>> = blocks
            .iter()
            .filter(|block| !applied.contains(block.header().height))
            .cloned()
            .collect();
        let mut segments = self.segments.lend();
        Ok(move || {
            for block in &unapplied {
                let (block_receives, block_spent) = project(block);
                receives.extend(block_receives);
                spent.extend(block_spent);
            }
            // segment writes, fsyncs, the manifest and any merges
            segments.commit((receives, spent), reached, tip)?;
            Ok(Landing { segments })
        })
    }

    async fn committed(&mut self, Landing { segments }: Landing) -> Result<(), StoreError> {
        self.durable = Durable::of(&segments);
        self.segments.restore(segments);
        self.non_finalized.land_below(self.durable.committed.extent);
        Ok(())
    }

    async fn reset(&mut self) -> Result<(), StoreError> {
        // segments untouched (commits finalised-only: no reorg reaches one)
        self.non_finalized = NonFinalizedRows::empty_at(self.finalized_height());

        Ok(())
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
    use zaino_persistence::fs::SimFs;
    use zaino_sync::Served;
    use zcash_transparent::address::TransparentAddress;

    use crate::{key::AddressKey, TransactionRef, TransparentAddressService};

    /// Service over the writer's current view (what the follower publishes after each step)
    fn served(writer: &TransparentAddressIndexWriter) -> TransparentAddressService {
        TransparentAddressService::new(Served::fixed(writer.view()), NetworkType::Regtest)
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

    /// `(finalized, applied)`
    fn extents(writer: &TransparentAddressIndexWriter) -> (u64, u64) {
        (u64::from(writer.finalized_height()), u64::from(writer.applied_height()))
    }

    /// `(height, txid)` of every transaction touching `address` in `[0, to]`
    fn touching(
        service: &TransparentAddressService,
        address: &TransparentAddress,
        to: u32,
    ) -> Vec<(u32, TransactionId)> {
        let found = service.transactions(address, h(0), h(to)).expect("transactions");
        found.into_iter().map(|found| (found.height, found.txid)).collect()
    }

    fn p2pkh(tag: u8) -> Vec<u8> {
        [&[0x76, 0xa9, 0x14][..], &[tag; 20], &[0x88, 0xac]].concat()
    }

    fn block(height: u32, transactions: Vec<Transaction>) -> Block {
        Block::new(
            BlockHeader::for_tests(
                height,
                [height as u8; 32],
                [height.wrapping_sub(1) as u8; 32],
                1_700_000_000 + height,
            ),
            transactions,
        )
    }

    fn tx(tag: u8, inputs: Vec<(u8, u32)>, outputs: Vec<(Vec<u8>, u64)>) -> Transaction {
        Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
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

    /// Ten one-block commits (9th launches a background merge of eight tier-0 segments per set,
    /// 10th lands it), crashed at every persistence point: each state reopens to a committed prefix
    /// with its one unspent output and balance exact, only listed segments, takes the next block
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
                Arc::new(block(
                    height,
                    vec![tx(height as u8 + 1, spends, vec![(p2pkh(0xa1), u64::from(height) + 1)])],
                ))
            })
            .collect();

        let fs = SimFs::recording();
        let open = |fs: Arc<SimFs>| {
            TransparentAddressIndexWriter::open(fs, Path::new("/ta"), NetworkType::Regtest)
        };
        // `(receives, spent)` segments listed
        let segments = |writer: &TransparentAddressIndexWriter| {
            let (receives, spent) = writer.segments.get().sets();
            (receives.pin().segments().len(), spent.pin().segments().len())
        };
        {
            let mut writer = open(fs.clone()).expect("open");
            for (acked, block) in (1u64..).zip(&chain[..10]) {
                let (receives, spent) = writer.segments.get().logs();
                receives.settle();
                spent.settle();
                zaino_sync::finalize_now(&mut writer, std::slice::from_ref(block))
                    .await
                    .expect("finalize");
                fs.set_tag(acked);
            }
            // receives: 8 merged + 9th + 10th; spent (from block 1): 8 merged + 10th
            assert_eq!(segments(&writer), (3, 2));
        }

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let mut writer =
                open(Arc::clone(&state.fs)).unwrap_or_else(|error| panic!("{label}: {error}"));
            let count = u64::from(writer.finalized_height());
            let acked = [state.tag, (state.tag + 1).min(10)];
            assert!(acked.contains(&count), "{label}: recovered {count}");
            let (receives, spent) = segments(&writer);
            for (set, listed) in [("receives", receives), ("spent", spent)] {
                let files = state.fs.list(&Path::new("/ta").join(set)).expect("list").len();
                assert_eq!(files, 2 * listed, "{label}: only listed {set} segments + checksums");
            }

            let expected = |count: u64| match count {
                0 => (Vec::new(), Zatoshis::ZERO),
                count => {
                    (vec![(count as u32 - 1, count)], Zatoshis::new(count).expect("in supply"))
                }
            };
            let observed = |service: &crate::TransparentAddressService| {
                let utxos = service
                    .utxos(&alice, h(0))
                    .expect("utxos")
                    .into_iter()
                    .map(|utxo| (utxo.height, utxo.value.as_u64()))
                    .collect::<Vec<_>>();
                (utxos, service.balance(&alice).expect("balance"))
            };
            assert_eq!(observed(&served(&writer)), expected(count), "{label}");

            let next =
                zaino_sync::finalize_now(&mut writer, std::slice::from_ref(&chain[count as usize]))
                    .await;
            next.unwrap_or_else(|error| panic!("{label}: {error}"));
            let after = observed(&served(&writer));
            assert_eq!(after, expected(count + 1), "{label}: next after recovery");
        }
    }

    #[derive(Debug, Clone)]
    enum Step {
        Apply,
        Finalize(usize),
        Reset,
        Reopen,
    }

    /// Per block: outputs `(address 0..3, zats)`, spend picks (index into what is unspent then)
    type BlockPlan = (Vec<(u8, u64)>, Vec<usize>);

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random spend graphs through random apply / finalize / reset / reopen sequences: after
        /// every step, each address's utxos, balance and transactions equal a naive UTXO set's
        #[test]
        fn random_histories_answer_like_a_naive_utxo_set(
            plans in proptest::collection::vec(
                (
                    proptest::collection::vec((0u8..3, 1u64..1_000), 0..3),
                    proptest::collection::vec(0usize..8, 0..3),
                ),
                1..10,
            ),
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
                .block_on(random_history(plans, steps));
        }
    }

    /// Outpoint → (address, zats, height received, height spent)
    type Ledger = Vec<((TransactionId, u32), (u8, u64, u32, Option<(u32, TransactionId)>))>;

    async fn random_history(plans: Vec<BlockPlan>, steps: Vec<Step>) {
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
            chain.push(Arc::new(block(
                height,
                vec![Transaction {
                    txid: txid(height),
                    transparent: TransparentData {
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
            )));
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

        let fs = SimFs::new();
        let open = || {
            TransparentAddressIndexWriter::open(fs.clone(), Path::new("/ta"), NetworkType::Regtest)
                .expect("open")
        };
        let mut writer = open();
        for (at, step) in steps.iter().enumerate() {
            let (applied, finalized) = (
                u64::from(writer.applied_height()) as usize,
                u64::from(writer.finalized_height()) as usize,
            );
            match *step {
                Step::Apply if applied < chain.len() => {
                    writer.apply(&chain[applied]).await.expect("apply");
                }
                Step::Finalize(count) if finalized < chain.len() => {
                    let end = (finalized + count).min(chain.len());
                    let write = writer.finalize(&chain[finalized..end]).await.expect("finalize");
                    let done = write().expect("written");
                    // the follower applies while a write is out only above an applied batch (tip)
                    if end <= applied && applied < chain.len() {
                        writer.apply(&chain[applied]).await.expect("apply, write out");
                    }
                    writer.committed(done).await.expect("landed");
                }
                Step::Reset => writer.reset().await.expect("reset"),
                Step::Reopen => {
                    drop(writer);
                    writer = open();
                }
                _ => {}
            }

            let applied = u64::from(writer.applied_height()) as u32;
            let service = served(&writer);
            for tag in 0..3u8 {
                let (utxos, balance, touched) = expected(tag, applied);
                let served = service
                    .utxos(&address(tag), h(0))
                    .expect("utxos")
                    .into_iter()
                    .map(|utxo| (utxo.height, utxo.txid, utxo.vout, utxo.value.as_u64()))
                    .collect::<Vec<_>>();
                assert_eq!(served, utxos, "step {at} {step:?}: utxos of {tag}");
                let served = service.balance(&address(tag)).expect("balance").as_u64();
                assert_eq!(served, balance, "step {at} {step:?}: balance of {tag}");
                if applied > 0 {
                    let served = service.transactions(&address(tag), h(0), h(applied - 1));
                    let served = served.expect("transactions");
                    assert_eq!(served, touched, "step {at} {step:?}: transactions of {tag}");
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
            assert_eq!(batched, one_by_one, "step {at} {step:?}: utxos of all");
            let balances: Vec<u64> = service
                .balances(&addresses)
                .expect("balances of all")
                .into_iter()
                .map(|balance| balance.as_u64())
                .collect();
            let one_by_one: Vec<u64> = tags.iter().map(|&tag| expected(tag, applied).1).collect();
            assert_eq!(balances, one_by_one, "step {at} {step:?}: balances of all");
        }
    }

    /// Receive in one segment, its spend in the next, queried across both, then again after a
    /// reopen (resumes at the committed height, every earlier segment still readable)
    ///
    /// - segment 0 = bulk-sync path (`finalize` alone, no `apply`), segment 1 = the reorg window
    #[tokio::test]
    async fn a_spend_in_a_later_segment_retires_a_utxo_written_by_an_earlier_one() {
        let fs = SimFs::new();
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);

        let mut writer =
            TransparentAddressIndexWriter::open(fs.clone(), Path::new("/ta"), NetworkType::Regtest)
                .expect("open");
        assert_eq!(u64::from(writer.applied_height()), 0, "an empty index starts at genesis");

        // segment 0, heights 0..=1: alice paid twice, bob once, + one opaque output
        let segment_0 = [
            Arc::new(block(
                0,
                vec![tx(
                    0x10,
                    vec![],
                    vec![(p2pkh(0xa1), 500), (p2pkh(0xb0), 70), (vec![0x6a, 0x01], 1)],
                )],
            )),
            Arc::new(block(1, vec![tx(0x11, vec![], vec![(p2pkh(0xa1), 300)])])),
        ];
        zaino_sync::finalize_now(&mut writer, &segment_0).await.expect("finalize segment 0");
        assert_eq!(extents(&writer), (2, 2), "skipping the nonfinalised tier carries both extents");

        let balance = served(&writer).balance(&alice).expect("balance");
        assert_eq!(balance, zat(800), "both receives unspent");

        // segment 1, height 2: alice's first output spent, paying bob
        // applied first → drained out of the nonfinalised tier, not re-projected
        let spend = Arc::new(block(2, vec![tx(0x20, vec![(0x10, 0)], vec![(p2pkh(0xb0), 490)])]));
        writer.apply(&spend).await.expect("apply 2");
        zaino_sync::finalize_now(&mut writer, &[spend]).await.expect("finalize segment 1");

        let service = served(&writer);
        let alice_balance = service.balance(&alice).expect("balance");
        assert_eq!(alice_balance, zat(300), "spend in segment 1 retires the receive in segment 0");

        let utxos = service.utxos(&alice, h(0)).expect("utxos");
        assert_eq!(utxos.len(), 1, "only the unspent receive: {utxos:?}");
        assert_eq!((utxos[0].height, utxos[0].txid, utxos[0].value), (1, txid(0x11), zat(300)));

        // paying and spending transactions both alice's, each once
        let expected = vec![(0, txid(0x10)), (1, txid(0x11)), (2, txid(0x20))];
        assert_eq!(touching(&service, &alice, 2), expected);

        // opaque outputs stored, not dropped (same fold answers for them)
        assert_eq!(service.balance_of(AddressKey::opaque()).expect("opaque balance"), zat(1));

        // reopen → committed height, both segments still read
        drop(writer);
        let mut resumed =
            TransparentAddressIndexWriter::open(fs.clone(), Path::new("/ta"), NetworkType::Regtest)
                .expect("reopen");
        assert_eq!(extents(&resumed), (3, 3), "resumes where it committed");

        let balances = |service: &TransparentAddressService| {
            (service.balance(&alice).expect("alice"), service.balance(&bob).expect("bob"))
        };
        assert_eq!(
            balances(&served(&resumed)),
            (zat(300), zat(560)),
            "segments = state, no replay"
        );

        // third segment continues the numbering, never overwrites segment 1
        let after_restart =
            Arc::new(block(3, vec![tx(0x30, vec![(0x11, 0)], vec![(p2pkh(0xa1), 290)])]));
        resumed.apply(&after_restart).await.expect("apply 3");
        zaino_sync::finalize_now(&mut resumed, &[after_restart]).await.expect("finalize segment 2");

        let resumed_service = served(&resumed);
        let alice_balance = resumed_service.balance(&alice).expect("balance");
        assert_eq!(alice_balance, zat(290), "pre-restart receive spendable post-restart");
        let recent = resumed_service.utxos(&alice, h(3)).expect("utxos").len();
        assert_eq!(recent, 1, "start_height filters the reply, not the spend resolution");
    }

    /// Receives and spends above the committed extent answer from the nonfinalised tier alone;
    /// `reset` drops all of it (segments untouched); answers then follow the re-applied branch
    #[tokio::test]
    async fn non_finalized_answers_above_the_committed_extent_and_a_reset_drops_only_it() {
        let fs = SimFs::new();
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);

        let mut writer =
            TransparentAddressIndexWriter::open(fs.clone(), Path::new("/ta"), NetworkType::Regtest)
                .expect("open");

        // height 0 alone durable, everything after it nonfinalised
        let genesis = Arc::new(block(0, vec![tx(0xc0, vec![], vec![(p2pkh(0xa1), 500)])]));
        writer.apply(&genesis).await.expect("apply 0");
        zaino_sync::finalize_now(&mut writer, &[genesis]).await.expect("finalize height 0");
        writer
            .apply(&Arc::new(block(1, vec![tx(0xd1, vec![], vec![(p2pkh(0xa1), 200)])])))
            .await
            .expect("apply 1");
        writer
            .apply(&Arc::new(block(2, vec![tx(0xd2, vec![(0xc0, 0)], vec![(p2pkh(0xb0), 490)])])))
            .await
            .expect("apply 2");

        let balances = |service: &TransparentAddressService| {
            (service.balance(&alice).expect("alice"), service.balance(&bob).expect("bob"))
        };
        assert_eq!(extents(&writer), (1, 3));

        // committed receive, nonfinalised spend: the probe crosses the boundary
        // bob: a receive nothing has committed is still an answer
        let service = served(&writer);
        assert_eq!(balances(&service), (zat(200), zat(490)));
        let utxo = crate::AddressUtxo { height: 1, txid: txid(0xd1), vout: 0, value: zat(200) };
        assert_eq!(service.utxos(&alice, h(0)).expect("utxos"), vec![utxo]);
        let expected = vec![(0, txid(0xc0)), (1, txid(0xd1)), (2, txid(0xd2))];
        assert_eq!(touching(&service, &alice, 2), expected);

        // branch loses: nonfinalised tier dropped whole, no segment touched, no fork height named
        writer.reset().await.expect("reset");
        assert_eq!(extents(&writer), (1, 1));
        let service = served(&writer);
        assert_eq!(balances(&service), (zat(500), zat(0)), "only durable height 0 survives");
        assert_eq!(touching(&service, &alice, 2), vec![(0, txid(0xc0))], "dropped: no rows");

        // winning branch = ordinary applies from the durable tip (the restart path)
        let winner = [
            Arc::new(block(1, vec![tx(0xd1, vec![], vec![(p2pkh(0xa1), 200)])])),
            Arc::new(block(2, vec![tx(0xe2, vec![(0xd1, 0)], vec![(p2pkh(0xb0), 190)])])),
        ];
        for block in &winner {
            writer.apply(block).await.expect("apply the winning branch");
        }

        let service = served(&writer);
        assert_eq!(service.balance(&alice).expect("balance"), zat(500));
        let expected = vec![(0, txid(0xc0)), (1, txid(0xd1)), (2, txid(0xe2))];
        assert_eq!(touching(&service, &alice, 2), expected, "orphan's txid gone, winner's there");

        // committing the winner changes no answer, only where it is read from: written but not
        // landed, its rows are on disk and still buffered, and each is counted once
        let write = writer.finalize(&winner).await.expect("finalize the winner");
        let done = write().expect("written");
        assert_eq!(extents(&writer), (1, 3), "durable moves only on landing");
        let unlanded = balances(&served(&writer));
        assert_eq!(unlanded, (zat(500), zat(190)), "written, not landed: one tier");
        writer.committed(done).await.expect("landed");
        assert_eq!(extents(&writer), (3, 3));
        let (applied, committed) = (balances(&service), balances(&served(&writer)));
        assert_eq!(applied, (zat(500), zat(190)), "view pinned before the commit");
        assert_eq!(committed, applied, "same answers from the segments");
    }

    /// Gap = dropped receives (later spends probe unwritten rows, the address keeps a balance it no
    /// longer has) → panic at the gap
    #[tokio::test]
    #[should_panic(expected = "blocks must arrive contiguously")]
    async fn a_gap_in_the_block_stream_panics() {
        let fs = SimFs::new();
        let mut writer =
            TransparentAddressIndexWriter::open(fs.clone(), Path::new("/ta"), NetworkType::Regtest)
                .expect("open");

        writer.apply(&Arc::new(block(0, vec![tx(0xc0, vec![], vec![])]))).await.expect("apply 0");
        let _ = writer.apply(&Arc::new(block(2, vec![tx(0xc2, vec![], vec![])]))).await;
    }
}
