//! transparent_address writer: the final stream → one [`fold`] per block → its store

use std::num::NonZeroUsize;

use tokio::sync::watch;
use zaino_persistence::{MapRead, Store};
use zaino_sync::{Committer, Final, Subscription};

use crate::{fold, TransparentAddressReader};

pub struct TransparentAddressIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: MapRead>> TransparentAddressIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::subscribe`: the committed view after every commit
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.store.committed()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    pub async fn run(mut self, mut blocks: Subscription<Final>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let applied = move |store: &mut S| {
                let network = store.schema().network;
                run.apply(store, |store, block| {
                    fold(&TransparentAddressReader::new(store.staged(), network), block)
                });
            };
            self.store.compute(applied).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use proptest::strategy::Strategy as _;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, View,
    };
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        Block, Height, OutPoint, Script, Transaction, TransactionId, TransparentData,
        TransparentOutput, Zatoshis,
    };
    use zaino_sync::{Folds, IndexerDataSink, Step};
    use zcash_protocol::consensus::NetworkType;
    use zcash_transparent::address::TransparentAddress;

    use super::*;
    use crate::{key::AddressKey, schema, TransactionRef};

    const NAME: &str = IndexKind::TransparentAddress.name();
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const NETWORK: NetworkType = NetworkType::Regtest;

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/ta"), &schema(NETWORK)).expect("open")
    }

    /// The writer over `store`, its final stream and committed view
    fn start(
        store: DiskStore,
        batch: NonZeroUsize,
    ) -> (IndexerDataSink<Final>, watch::Receiver<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = TransparentAddressIndexWriter::new(store, batch);
        let committed = writer.committed();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, committed, running)
    }

    /// `block` as the NFS sends it: unfolded, or folded (its `Changes` = this index's own fold)
    fn step(block: &Block, folded: bool) -> Step<Final> {
        let folds = folded.then(|| {
            let mut folds = Folds::default();
            let view = DiskEngine::new(SimFs::new()).open(Path::new("/x"), &schema(NETWORK));
            let parent = TransparentAddressReader::new(view.expect("open").view(), NETWORK);
            folds.insert(IndexKind::TransparentAddress, fold(&parent, block));
            Arc::new(folds)
        });
        let (height, block) = (block.header().height, Arc::new(block.clone()));
        Step::Apply { height, data: Arc::new(Final { block, folds }) }
    }

    /// `committed` at `tip` (`None` = nothing)
    async fn reached(committed: &mut watch::Receiver<DiskView>, tip: Option<u32>) {
        let at = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height)) == tip;
        committed.wait_for(at).await.expect("writer alive");
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

    fn p2pkh(tag: u8) -> Vec<u8> {
        [&[0x76, 0xa9, 0x14][..], &[tag; 20], &[0x88, 0xac]].concat()
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

    /// Ten one-block commits (batch = 1 byte: each block commits as it arrives; the 9th launches
    /// a background merge), crashed at every persistence point: each state reopens to a committed
    /// prefix with its one unspent output and balance exact, takes the next block
    ///
    /// - block `h` pays alice `h + 1` zats (vout 0) and spends block `h - 1`'s payment
    #[tokio::test]
    async fn every_crash_state_of_commits_and_a_merge_reopens_to_a_committed_prefix() {
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let chain: Vec<Arc<Block>> = linked((0u32..11).map(|height| {
            let spends = match height {
                0 => Vec::new(),
                _ => vec![(height as u8, 0)],
            };
            let paid = vec![(p2pkh(0xa1), u64::from(height) + 1)];
            vec![tx(height as u8 + 1, spends, paid)]
        }));
        let fs = SimFs::recording();
        {
            let (sink, mut committed, running) = start(open(&fs), NonZeroUsize::MIN);
            for (acked, block) in (1u64..).zip(&chain[..10]) {
                sink.send(step(block, false)).await;
                reached(&mut committed, Some(u32::from(block.header().height))).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("stops at Shutdown");
        }

        let expected = |count: u64| match count {
            0 => (Vec::new(), Zatoshis::ZERO),
            count => (vec![(h(count as u32 - 1), count)], zat(count)),
        };
        let observed = |view: DiskView| {
            let reader = TransparentAddressReader::new(view, NETWORK);
            let utxos = reader.utxos(&alice, h(0)).expect("utxos");
            let utxos = utxos.into_iter().map(|utxo| (utxo.height, utxo.value.as_u64()));
            (utxos.collect::<Vec<_>>(), reader.balance(&alice).expect("balance"))
        };
        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs);
            // one block per commit: blocks held = commits recovered
            let count = store.view().tip().map_or(0, |tip| u64::from(tip.height) + 1);
            let acked = [state.tag, (state.tag + 1).min(10)];
            assert!(acked.contains(&count), "{label}: recovered {count}");
            assert_eq!(observed(store.view()), expected(count), "{label}");

            let (sink, _committed, running) = start(store, QUEUE);
            sink.send(step(&chain[count as usize], false)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let after = observed(open(&state.fs).view());
            assert_eq!(after, expected(count + 1), "{label}: next after recovery");
        }
    }

    /// Receive in one segment, its spend in the next, queried across both, after restarts
    ///
    /// - 0, 1 unfolded (bulk: committed once the stream idles), 2 folded (the tip)
    /// - restart: 1 and 2 resent (held: skipped), 3 folded
    #[tokio::test(start_paused = true)]
    async fn a_spend_in_a_later_segment_retires_a_utxo_and_a_restart_skips_what_it_holds() {
        let fs = SimFs::new();
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);
        let blocks = linked(vec![
            vec![tx(0x10, vec![], vec![(p2pkh(0xa1), 500), (p2pkh(0xb0), 70), (vec![0x6a, 1], 1)])],
            vec![tx(0x11, vec![], vec![(p2pkh(0xa1), 300)])],
            vec![tx(0x20, vec![(0x10, 0)], vec![(p2pkh(0xb0), 490)])],
            vec![tx(0x30, vec![(0x11, 0)], vec![(p2pkh(0xa1), 290)])],
        ]);
        let reader = |committed: &watch::Receiver<DiskView>| {
            TransparentAddressReader::new(committed.borrow().clone(), NETWORK)
        };

        let (sink, mut committed, running) = start(open(&fs), QUEUE);
        for block in &blocks[..2] {
            sink.send(step(block, false)).await;
        }
        reached(&mut committed, Some(1)).await;
        assert_eq!(reader(&committed).balance(&alice), Ok(zat(800)), "both receives unspent");
        sink.send(step(&blocks[2], true)).await;
        reached(&mut committed, Some(2)).await;
        let at_two = reader(&committed);
        assert_eq!(at_two.balance(&alice), Ok(zat(300)), "2's spend retires 0's receive");
        let utxos = at_two.utxos(&alice, h(0)).expect("utxos");
        let utxos: Vec<_> =
            utxos.iter().map(|utxo| (utxo.height, utxo.outpoint.txid, utxo.value)).collect();
        assert_eq!(utxos, [(h(1), txid(0x11), zat(300))], "only the unspent receive");
        // paying and spending transactions both alice's, each once
        let touching = at_two.transactions(&alice, h(0), h(2)).expect("transactions");
        let expected = [(0, 0x10), (1, 0x11), (2, 0x20)]
            .map(|(height, tag)| TransactionRef { height: h(height), txid: txid(tag) });
        assert_eq!(touching, expected);
        // opaque outputs stored, not dropped (same fold answers for them)
        assert_eq!(at_two.balance_of(AddressKey::opaque()), Ok(zat(1)));
        sink.shutdown();
        running.await.expect("stops at Shutdown");

        let (sink, mut committed, running) = start(open(&fs), QUEUE);
        assert_eq!(reader(&committed).balance(&bob), Ok(zat(560)), "resumed at 2, no replay");
        for (block, folded) in [(&blocks[1], false), (&blocks[2], true), (&blocks[3], true)] {
            sink.send(step(block, folded)).await;
        }
        reached(&mut committed, Some(3)).await;
        let at_three = reader(&committed);
        assert_eq!(at_three.balance(&alice), Ok(zat(290)), "pre-restart receive spent after it");
        assert_eq!(at_three.balance(&bob), Ok(zat(560)), "resent 2 not applied twice");
        let recent = at_three.utxos(&alice, h(3)).expect("utxos").len();
        assert_eq!(recent, 1, "start_height filters the reply, not the spend resolution");
        sink.shutdown();
        running.await.expect("stops at Shutdown");
    }

    #[derive(Debug, Clone)]
    enum Move {
        /// Next blocks: unfolded until `Fold`, folded after it
        Send(usize),
        /// Bulk → tip handoff: every later block folded
        Fold,
        /// Shutdown, reopen, resend from one below the committed tip (held: skipped)
        Reopen,
    }

    /// Per block: outputs `(address 0..3, zats)`, spend picks (index into what is unspent then)
    type BlockPlan = (Vec<(u8, u64)>, Vec<usize>);

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random spend graphs through random final streams (bulk, tip, restarts): once every
        /// move commits, each address's utxos, balance and transactions equal a naive UTXO set's
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
                    4 => (1usize..=3).prop_map(Move::Send),
                    1 => proptest::strategy::Just(Move::Fold),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
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
        let mut planned = Vec::new();
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
            planned.push(vec![Transaction {
                txid: txid(height),
                transparent: TransparentData {
                    coinbase: false,
                    inputs: inputs.iter().map(|&(txid, vout)| OutPoint { txid, vout }).collect(),
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
            }]);
        }
        let chain = linked(planned);

        // the model's answers once `held` blocks are indexed
        let expected = |tag: u8, held: u32| {
            let visible = |height: u32| height < held;
            let mut utxos = Vec::new();
            let mut touched = Vec::new();
            for &((txid, vout), (owner, zats, received, spent)) in &ledger {
                if owner != tag || !visible(received) {
                    continue;
                }
                touched.push(TransactionRef { height: h(received), txid });
                match spent.filter(|&(height, _)| visible(height)) {
                    Some((height, spender)) => {
                        touched.push(TransactionRef { height: h(height), txid: spender })
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
        let (mut sink, mut committed, mut running) = start(open(&fs), NonZeroUsize::MIN);
        let (mut sent, mut folding) = (0usize, false);
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    for block in chain.iter().skip(sent).take(count) {
                        sink.send(step(block, folding)).await;
                        sent += 1;
                    }
                }
                Move::Fold => folding = true,
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("stops at Shutdown");
                    (sink, committed, running) = start(open(&fs), NonZeroUsize::MIN);
                    folding = false;
                    if let Some(held) = sent.checked_sub(1) {
                        sink.send(step(&chain[held], false)).await;
                    }
                }
            }
            reached(&mut committed, sent.checked_sub(1).map(|last| last as u32)).await;

            let held = sent as u32;
            let reader = TransparentAddressReader::new(committed.borrow().clone(), NETWORK);
            for tag in 0..3u8 {
                let (utxos, balance, touched) = expected(tag, held);
                let served = reader.utxos(&address(tag), h(0)).expect("utxos");
                let served: Vec<_> = served
                    .into_iter()
                    .map(|utxo| {
                        let OutPoint { txid, vout } = utxo.outpoint;
                        (u32::from(utxo.height), txid, vout, utxo.value.as_u64())
                    })
                    .collect();
                assert_eq!(served, utxos, "move {at} {next:?}: utxos of {tag}");
                let served = reader.balance(&address(tag)).expect("balance").as_u64();
                assert_eq!(served, balance, "move {at} {next:?}: balance of {tag}");
                if held > 0 {
                    let served = reader.transactions(&address(tag), h(0), h(held - 1));
                    assert_eq!(served, Ok(touched), "move {at} {next:?}: transactions of {tag}");
                }
            }

            // every address in one request, repeats included: one batched spend lookup, same
            // answers as one address at a time
            let tags = [2u8, 0, 1, 0];
            let addresses = tags.map(address);
            let batched: Vec<Vec<_>> = reader
                .utxos_of(&addresses, h(0))
                .expect("utxos of all")
                .into_iter()
                .map(|utxos| {
                    utxos
                        .into_iter()
                        .map(|utxo| {
                            let OutPoint { txid, vout } = utxo.outpoint;
                            (u32::from(utxo.height), txid, vout, utxo.value.as_u64())
                        })
                        .collect()
                })
                .collect();
            let one_by_one: Vec<Vec<_>> = tags.iter().map(|&tag| expected(tag, held).0).collect();
            assert_eq!(batched, one_by_one, "move {at} {next:?}: utxos of all");
            let balances = reader.balances(&addresses).expect("balances of all");
            let balances: Vec<u64> = balances.into_iter().map(|zats| zats.as_u64()).collect();
            let one_by_one: Vec<u64> = tags.iter().map(|&tag| expected(tag, held).1).collect();
            assert_eq!(balances, one_by_one, "move {at} {next:?}: balances of all");
        }
        sink.shutdown();
        running.await.expect("stops at Shutdown");
    }
}
