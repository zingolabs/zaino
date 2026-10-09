//! transparent_address writer: the final stream → one [`fold`] per block → its store

use zaino_persistence::{BlockChanges, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{apply, blocking, commit, held, IndexHandle, IndexPublisher, Subscription};

use crate::{
    address::address_key,
    key::{encode_receive, encode_spend, ReceiveKey, ReceiveRow, Spend},
    TransparentAddressReader, RECEIVES, SPENT,
};

pub struct TransparentAddressIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: MapRead>> TransparentAddressIndexWriter<S> {
    /// Over `store` (opened with [`TABLES`](crate::TABLES))
    pub fn new(store: S) -> Self {
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: committed view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    ///
    /// - commits after the final tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            store = blocking(move || {
                for (height, block) in &run.blocks {
                    if !held(&store, *height) {
                        let mut changes = store.changes(block.at());
                        fold(&TransparentAddressReader::new(store.staged()), block, &mut changes);
                        apply(&mut store, changes);
                    }
                }
                if run.finalized {
                    commit(&mut store);
                }
                store
            })
            .await;
            publisher.publish(&store);
        }
        store = blocking(move || {
            commit(&mut store);
            store
        })
        .await;
        publisher.publish(&store);
    }
}

/// `block` onto `parent`: its `receives` and `spent` rows
///
/// - projection, no lookups: a spend keyed by its outpoint (already in the block), never resolved
///   to an address (`docs/design/index-data-structures.md` §5)
pub fn fold<V: MapRead>(
    parent: &TransparentAddressReader<V>,
    block: &Block,
    out: &mut BlockChanges,
) {
    out.assert_next(parent.view().tip(), block);
    let height = u32::from(block.header().height);
    for tx in block.transactions() {
        // coinbase inputs elided upstream (`zaino-source` decode.rs)
        let mut spent = out.map(SPENT);
        for input in &tx.transparent.inputs {
            spent.insert(&input.encode(), &encode_spend(&Spend { height, spender: tx.txid }));
        }
        let mut receives = out.map(RECEIVES);
        for (vout, output) in (0u32..).zip(&tx.transparent.outputs) {
            let address = address_key(output.script.as_bytes());
            let key = ReceiveKey { address, height, txid: tx.txid, vout };
            let (key, value) = encode_receive(&ReceiveRow { key, value: output.value });
            receives.insert(&key, &value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path, sync::Arc};

    use proptest::strategy::Strategy as _;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema, View,
    };
    use zaino_primitives::testing::{h, outpoint, p2pkh, BlockBuilder, MockChain};
    use zaino_primitives::types::{OutPoint, Script, TransactionId, Zatoshis};
    use zaino_sync::{IndexerDataSink, Step};
    use zcash_protocol::consensus::NetworkType;
    use zcash_transparent::address::TransparentAddress;

    use super::*;
    use crate::{key::AddressKey, TransactionRef, FORMAT, TABLES};

    const NAME: &str = IndexKind::TransparentAddress.name();
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const SCHEMA: Schema =
        Schema::new(IndexKind::TransparentAddress, FORMAT, NetworkType::Regtest, TABLES);

    /// `write_buffer` = MIN: every applied block committed by the store itself
    fn open(fs: &Arc<SimFs>, write_buffer: NonZeroUsize) -> DiskStore {
        DiskEngine::new(fs.clone(), zaino_persistence::LsmConfig::default())
            .open(Path::new("/ta"), &SCHEMA, write_buffer)
            .expect("open")
    }

    /// Writer over `store`, its final stream and handle
    fn start(
        store: DiskStore,
    ) -> (IndexerDataSink<Block>, IndexHandle<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = TransparentAddressIndexWriter::new(store);
        let handle = writer.handle();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, handle, running)
    }

    fn step(block: &Arc<Block>) -> Step<Block> {
        Step::Apply { height: block.header().height, data: Arc::clone(block) }
    }

    /// The chain's final tip: the writer commits after it
    fn finalized(block: &Arc<Block>) -> Step<Block> {
        Step::Finalized { height: block.header().height, data: Arc::clone(block) }
    }

    /// Block 1 spends alice's block-0 receive, paying bob: its `BlockChanges` = the spend under the
    /// outpoint + bob's receive (nothing looked up); read over the parent + it, alice's receive
    /// spent by block 1, bob's two unspent, the opaque output kept
    #[test]
    fn a_spend_folded_on_its_parent_retires_the_receive_the_parent_holds() {
        let zat = |n: u64| Zatoshis::new(n).expect("in supply");
        let opaque = Script::new(vec![0x6a]);
        let mut chain = MockChain::regtest().genesis_with(|b| {
            b.coinbase(|c| {
                c.txid([0x10; 32])
                    .pay(&p2pkh([0xa1; 20]), 500)
                    .pay(&p2pkh([0xb0; 20]), 70)
                    .pay(&opaque, 1)
            })
        });
        let paid = outpoint([0x10; 32], 0);
        let one =
            chain.mine(|b| b.tx(|t| t.txid([0x20; 32]).spend(paid).pay(&p2pkh([0xb0; 20]), 490)));
        let blocks = chain.blocks(one);
        let (alice, bob) = (AddressKey::p2pkh([0xa1; 20]), AddressKey::p2pkh([0xb0; 20]));

        let mut store = open(&SimFs::new(), NonZeroUsize::MAX);
        let mut genesis = store.changes(blocks[0].at());
        fold(&TransparentAddressReader::new(store.staged()), &blocks[0], &mut genesis);
        store.apply(genesis);
        let mut changes = store.changes(blocks[1].at());
        fold(&TransparentAddressReader::new(store.staged()), &blocks[1], &mut changes);

        let spender = TransactionId::from([0x20; 32]);
        let spend = encode_spend(&Spend { height: 1, spender });
        let spent: Vec<_> = changes.inserts(SPENT).map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        assert_eq!(spent, vec![(paid.encode().to_vec(), spend.to_vec())], "spent row");
        let bob_receive = ReceiveRow {
            key: ReceiveKey { address: bob, height: 1, txid: spender, vout: 0 },
            value: zat(490),
        };
        let (key, value) = encode_receive(&bob_receive);
        let receives: Vec<_> = changes.inserts(RECEIVES).collect();
        assert_eq!(receives, vec![(&key[..], &value[..])], "receives row");

        store.apply(changes);
        let read = TransparentAddressReader::new(store.staged());
        let unspent =
            read.unspent(&[alice, bob, AddressKey::opaque()], 0, usize::MAX).expect("rows");
        let values: Vec<Vec<(u32, u64)>> = unspent
            .iter()
            .map(|rows| rows.iter().map(|row| (row.key.height, row.value.as_u64())).collect())
            .collect();
        assert_eq!(values, vec![vec![], vec![(0, 70), (1, 490)], vec![(0, 1)]], "unspent");
        let alice_receive = read.receives(alice, 0, usize::MAX).expect("rows")[0].key;
        assert_eq!(read.spends_of(&[alice_receive]), vec![Some(Spend { height: 1, spender })]);
    }

    /// `handle`'s durable tip at `tip` (`None` = nothing)
    async fn reached(handle: &mut IndexHandle<DiskView>, tip: Option<u32>) {
        while handle.tip().map(|tip| u32::from(tip.height)) != tip {
            assert!(handle.changed().await, "writer alive");
        }
    }

    /// Ten one-block commits (write_buffer = 1 byte: each block commits as it arrives; the 9th
    /// launches a background merge), crashed at every persistence point: each state reopens to a
    /// committed prefix with its one unspent output and balance exact, takes the next block
    ///
    /// - block `h`'s coinbase pays alice `h + 1` zats (vout 0); its tx spends block `h - 1`'s
    #[tokio::test]
    async fn every_crash_state_of_commits_and_a_merge_reopens_to_a_committed_prefix() {
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let pays_alice = p2pkh([0xa1; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([1; 32]).pay(&pays_alice, 1)));
        for height in 1u8..=10 {
            let paid = u64::from(height) + 1;
            chain.mine(|b| {
                b.coinbase(|c| c.txid([height + 1; 32]).pay(&pays_alice, paid))
                    .tx(|t| t.spend(outpoint([height; 32], 0)))
            });
        }
        let blocks = chain.blocks(chain.tip());
        let fs = SimFs::recording();
        {
            let (sink, mut handle, running) = start(open(&fs, NonZeroUsize::MIN));
            for (acked, block) in (1u64..).zip(&blocks[..10]) {
                sink.send(step(block)).await;
                reached(&mut handle, Some(u32::from(block.header().height))).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("stops at Shutdown");
        }

        let expected = |count: u64| match count {
            0 => (Vec::new(), 0),
            count => (vec![(h(count as u32 - 1), count)], count),
        };
        let observed = |view: DiskView| {
            let reader = TransparentAddressReader::new(view);
            let utxos = reader.utxos(&alice, h(0)).expect("utxos");
            let utxos = utxos.into_iter().map(|utxo| (utxo.height, utxo.value.as_u64()));
            (utxos.collect::<Vec<_>>(), reader.balance(&alice).expect("balance").as_u64())
        };
        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs, NonZeroUsize::MAX);
            // one block per commit: blocks held = commits recovered
            let count = store.committed().tip().map_or(0, |tip| u64::from(tip.height) + 1);
            let acked = [state.tag, (state.tag + 1).min(10)];
            assert!(acked.contains(&count), "{label}: recovered {count}");
            assert_eq!(observed(store.committed()), expected(count), "{label}");

            let (sink, _handle, running) = start(store);
            sink.send(step(&blocks[count as usize])).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let after = observed(open(&state.fs, NonZeroUsize::MAX).committed());
            assert_eq!(after, expected(count + 1), "{label}: next after recovery");
        }
    }

    /// Receive in one segment, its spend in the next, queried across both, after restarts
    ///
    /// - 0, 1 (`Finalized`: committed there), then 2 (`Finalized`)
    /// - restart: 1 and 2 resent (held: skipped), then 3 (`Finalized`)
    #[tokio::test]
    async fn a_spend_in_a_later_segment_retires_a_utxo_and_a_restart_skips_what_it_holds() {
        let fs = SimFs::new();
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let bob = TransparentAddress::PublicKeyHash([0xb0; 20]);
        let (pays_alice, pays_bob) = (p2pkh([0xa1; 20]), p2pkh([0xb0; 20]));
        let mut chain = MockChain::regtest().genesis_with(|b| {
            b.coinbase(|c| {
                c.txid([0x10; 32])
                    .pay(&pays_alice, 500)
                    .pay(&pays_bob, 70)
                    .pay(&Script::new(vec![0x6a, 1]), 1)
            })
        });
        chain.mine(|b| b.coinbase(|c| c.txid([0x11; 32]).pay(&pays_alice, 300)));
        chain.mine(|b| {
            b.tx(|t| t.txid([0x20; 32]).spend(outpoint([0x10; 32], 0)).pay(&pays_bob, 490))
        });
        let tip = chain.mine(|b| {
            b.tx(|t| t.txid([0x30; 32]).spend(outpoint([0x11; 32], 0)).pay(&pays_alice, 290))
        });
        let blocks = chain.blocks(tip);
        let reader = |handle: &IndexHandle<DiskView>| TransparentAddressReader::new(handle.view());
        let zats = |balance: Result<Zatoshis, _>| balance.map(Zatoshis::as_u64);

        let (sink, mut handle, running) = start(open(&fs, NonZeroUsize::MAX));
        sink.send(step(&blocks[0])).await;
        sink.send(finalized(&blocks[1])).await;
        reached(&mut handle, Some(1)).await;
        assert_eq!(zats(reader(&handle).balance(&alice)), Ok(800), "both receives unspent");
        sink.send(finalized(&blocks[2])).await;
        reached(&mut handle, Some(2)).await;
        let at_two = reader(&handle);
        assert_eq!(zats(at_two.balance(&alice)), Ok(300), "2's spend retires 0's receive");
        let utxos = at_two.utxos(&alice, h(0)).expect("utxos");
        let utxos: Vec<_> = utxos
            .iter()
            .map(|utxo| (utxo.height, utxo.outpoint.txid, utxo.value.as_u64()))
            .collect();
        let received = TransactionId::from([0x11; 32]);
        assert_eq!(utxos, [(h(1), received, 300)], "only the unspent receive");
        // paying and spending transactions both alice's, each once
        let touching = at_two.transactions(&alice, h(0), h(2)).expect("transactions");
        let expected = [(0, 0x10), (1, 0x11), (2, 0x20)].map(|(height, tag)| TransactionRef {
            height: h(height),
            txid: TransactionId::from([tag; 32]),
        });
        assert_eq!(touching, expected);
        // opaque outputs stored, not dropped (same fold answers for them)
        assert_eq!(zats(at_two.balance_of(AddressKey::opaque())), Ok(1));
        sink.shutdown();
        running.await.expect("stops at Shutdown");

        let (sink, mut handle, running) = start(open(&fs, NonZeroUsize::MAX));
        assert_eq!(zats(reader(&handle).balance(&bob)), Ok(560), "resumed at 2, no replay");
        for block in &blocks[1..3] {
            sink.send(step(block)).await;
        }
        sink.send(finalized(&blocks[3])).await;
        reached(&mut handle, Some(3)).await;
        let at_three = reader(&handle);
        assert_eq!(zats(at_three.balance(&alice)), Ok(290), "pre-restart receive spent after it");
        assert_eq!(zats(at_three.balance(&bob)), Ok(560), "resent 2 not applied twice");
        let recent = at_three.utxos(&alice, h(3)).expect("utxos").len();
        assert_eq!(recent, 1, "start_height filters the reply, not the spend resolution");
        sink.shutdown();
        running.await.expect("stops at Shutdown");
    }

    /// - `Send(n)`: next `n` blocks
    /// - `Reopen`: shutdown, reopen, resend from one below the tip (held: skipped)
    #[derive(Debug, Clone)]
    enum Move {
        Send(usize),
        Reopen,
    }

    /// Per block: outputs `(address 0..3, zats)`, spend picks (index into what is unspent then)
    type BlockPlan = (Vec<(u8, u64)>, Vec<usize>);

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random spend graphs through random final streams (runs, restarts): once every move
        /// commits, each address's utxos, balance and transactions equal a naive UTXO set's
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

    /// One block's tx: spends, pays `(address, zats)`
    type Planned = (Vec<OutPoint>, Vec<(u8, u64)>);

    async fn random_history(plans: Vec<BlockPlan>, moves: Vec<Move>) {
        let txid = |height: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&(height + 1).to_le_bytes());
            TransactionId::from(bytes)
        };
        let address = |tag: u8| TransparentAddress::PublicKeyHash([0xa0 + tag; 20]);

        // ledger first (spends pick from what is unspent then); block h = one tx `txid(h)`, its
        // inputs + outputs, value balanced through a faucet output (untracked address 3, vout last)
        const FAUCET: u8 = 3;
        let scripts = [0u8, 1, 2, 0x5f].map(|tag| p2pkh([0xa0 + tag; 20]));
        let mut ledger: Ledger = Vec::new();
        let mut planned: Vec<Planned> = Vec::new();
        let mut faucet = (outpoint([0xfa; 32], 0), 1_000_000_000u64);
        for (height, (outputs, picks)) in (0u32..).zip(&plans) {
            let mut spends = vec![faucet.0];
            let mut change = faucet.1;
            for pick in picks {
                let unspent: Vec<usize> =
                    (0..ledger.len()).filter(|&at| ledger[at].1 .3.is_none()).collect();
                if let Some(&at) = unspent.get(pick % unspent.len().max(1)) {
                    ledger[at].1 .3 = Some((height, txid(height)));
                    let (txid, vout) = ledger[at].0;
                    spends.push(OutPoint { txid, vout });
                    change += ledger[at].1 .1;
                }
            }
            for (vout, &(tag, zats)) in (0u32..).zip(outputs) {
                ledger.push(((txid(height), vout), (tag, zats, height, None)));
                change -= zats;
            }
            faucet = (OutPoint { txid: txid(height), vout: outputs.len() as u32 }, change);
            planned.push((spends, [&outputs[..], &[(FAUCET, change)]].concat()));
        }
        let block = |b: BlockBuilder, height: u32, (spends, pays): &Planned| {
            b.tx(|t| {
                let t = t.txid(<[u8; 32]>::from(txid(height)));
                let t = spends.iter().fold(t, |t, prevout| t.spend(*prevout));
                pays.iter().fold(t, |t, &(tag, zats)| t.pay(&scripts[usize::from(tag)], zats))
            })
        };
        let mut chain = MockChain::regtest().genesis_with(|b| {
            let b = b.coinbase(|c| c.txid([0xfa; 32]).pay(&scripts[3], 1_000_000_000));
            block(b, 0, &planned[0])
        });
        for (height, plan) in (1u32..).zip(&planned[1..]) {
            chain.mine(|b| block(b, height, plan));
        }
        let blocks = chain.blocks(chain.tip());

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
        let (mut sink, mut handle, mut running) = start(open(&fs, NonZeroUsize::MIN));
        let mut sent = 0usize;
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    for block in blocks.iter().skip(sent).take(count) {
                        sink.send(step(block)).await;
                        sent += 1;
                    }
                }
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("stops at Shutdown");
                    (sink, handle, running) = start(open(&fs, NonZeroUsize::MIN));
                    if let Some(held) = sent.checked_sub(1) {
                        sink.send(step(&blocks[held])).await;
                    }
                }
            }
            reached(&mut handle, sent.checked_sub(1).map(|last| last as u32)).await;

            let held = sent as u32;
            let reader = TransparentAddressReader::new(handle.view());
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
