//! transparent_address writer: the final stream → one [`fold`] per block → its store

use std::time::{Duration, Instant};

use zaino_persistence::{BlockChanges, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Subscription,
};

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
            let started = Instant::now();
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                for (height, block) in &run.blocks {
                    if !held(&store, *height) {
                        let mut changes = store.changes(block.at());
                        fold(&TransparentAddressReader::new(store.staged()), block, &mut changes);
                        write += apply(&mut store, changes);
                    }
                }
                if run.finalized {
                    write += commit(&mut store);
                }
                (store, write)
            })
            .await;
            publisher.publish(&store);
            ran(&store, started, write);
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
/// - projection, no lookups: a spend keyed by its outpoint (all a transparent input carries),
///   never resolved to an address
pub fn fold<V: MapRead>(
    parent: &TransparentAddressReader<V>,
    block: &Block,
    out: &mut BlockChanges,
) {
    assert_next(out, parent.view().tip(), block);
    let height = u32::from(block.header().height);
    for tx in block.transactions() {
        // coinbase inputs elided upstream (`zaino-source` decode.rs)
        let mut spent = out.map(SPENT);
        for input in &tx.transparent.inputs {
            spent.insert(&input.encode(), &encode_spend(&Spend { height, spender: tx.txid }));
        }
        let mut receives = out.map(RECEIVES);
        for (vout, output) in (0u32..).zip(&tx.transparent.outputs) {
            let address = address_key(&output.script.0 .0);
            let key = ReceiveKey { address, height, txid: tx.txid, vout };
            let (key, value) = encode_receive(&ReceiveRow { key, value: output.value });
            receives.insert(&key, &value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, IndexKind, Schema, View};
    use zaino_primitives::testing::{h, outpoint, p2pkh, BlockBuilder, MockChain};
    use zaino_primitives::types::{OutPoint, TransactionId};
    use zaino_sync::testing::{
        block_on_paused, finalized, moves, open, step, FinalStream, Move, Writer, QUEUE,
    };
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::value::Zatoshis;
    use zcash_script::script::Code;
    use zcash_transparent::address::{Script, TransparentAddress};

    use super::*;
    use crate::{key::AddressKey, AddressUtxo, TransactionRef, FORMAT, TABLES};

    const SCHEMA: Schema =
        Schema::new(IndexKind::TransparentAddress, FORMAT, NetworkType::Regtest, TABLES);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = TransparentAddressIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
    }

    /// Block 1 spends alice's block-0 receive, paying bob: its `BlockChanges` = the spend under the
    /// outpoint + bob's receive (nothing looked up); read over the parent + it, alice's receive
    /// spent by block 1, bob's two unspent, the opaque output kept
    #[test]
    fn a_spend_folded_on_its_parent_retires_the_receive_the_parent_holds() {
        let zat = |n: u64| Zatoshis::from_u64(n).expect("in supply");
        let opaque = Script(Code(vec![0x6a]));
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

        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
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
            .map(|rows| rows.iter().map(|row| (row.key.height, row.value.into_u64())).collect())
            .collect();
        assert_eq!(values, vec![vec![], vec![(0, 70), (1, 490)], vec![(0, 1)]], "unspent");
        let alice_receive = read.receives(alice, 0, usize::MAX).expect("rows")[0].key;
        assert_eq!(read.spends_of(&[alice_receive]), vec![Some(Spend { height: 1, spender })]);
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
        let (fs, recording) = SimFs::recording();
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
        for (acked, block) in (1u64..).zip(&blocks[..10]) {
            writer.send(step(block)).await;
            writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        writer.stop().await.expect("stops at Shutdown");

        // `count` blocks held: alice's one unspent = block `count - 1`'s coinbase
        let expected = |count: u8| {
            let balance = Zatoshis::from_u64(u64::from(count)).expect("in supply");
            let unspent = (count > 0).then(|| AddressUtxo {
                outpoint: outpoint([count; 32], 0),
                height: h(u32::from(count) - 1),
                value: balance,
            });
            (Ok(vec![Vec::from_iter(unspent)]), Ok(vec![balance]))
        };
        let observed = |view: DiskView| {
            let reader = TransparentAddressReader::new(view);
            (reader.utxos_of(&[alice], h(0)), reader.balances(&[alice]))
        };
        let states = recording.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MAX);
            // one block per commit: blocks held = commits recovered
            let count = store.committed().tip().map_or(0, |tip| u32::from(tip.height) as u8 + 1);
            let acked = [state.tag, (state.tag + 1).min(10)];
            assert!(acked.contains(&u64::from(count)), "{label}: recovered {count}");
            assert_eq!(observed(store.committed()), expected(count), "{label}");

            let writer = start(store);
            writer.send(step(&blocks[usize::from(count)])).await;
            writer.stop().await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let after = observed(open(&state.fs, &SCHEMA, NonZeroUsize::MAX).committed());
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
                    .pay(&Script(Code(vec![0x6a, 1])), 1)
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
        let zats = |zats: u64| Zatoshis::from_u64(zats).expect("in supply");
        let reader =
            |writer: &Writer<DiskView>| TransparentAddressReader::new(writer.handle().view());

        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        writer.send(step(&blocks[0])).await;
        writer.send(finalized(&blocks[1])).await;
        writer.durable_at(h(1)).await;
        let both_unspent = reader(&writer).balances(&[alice]);
        assert_eq!(both_unspent, Ok(vec![zats(800)]), "both receives unspent");
        writer.send(finalized(&blocks[2])).await;
        writer.durable_at(h(2)).await;
        let at_two = reader(&writer);
        let balances = at_two.balances(&[alice, bob]);
        assert_eq!(balances, Ok(vec![zats(300), zats(560)]), "2's spend retires 0's receive");
        let unspent =
            AddressUtxo { outpoint: outpoint([0x11; 32], 0), height: h(1), value: zats(300) };
        let utxos = at_two.utxos_of(&[alice], h(0));
        assert_eq!(utxos, Ok(vec![vec![unspent]]), "only the unspent receive");
        // paying and spending transactions both alice's, each once
        let touching = at_two.transactions(&alice, h(0), h(2)).expect("transactions");
        let expected = [(0, 0x10), (1, 0x11), (2, 0x20)].map(|(height, tag)| TransactionRef {
            height: h(height),
            txid: TransactionId::from([tag; 32]),
        });
        assert_eq!(touching, expected);
        // opaque outputs stored, not dropped (same fold answers for them)
        assert_eq!(at_two.balances_of(&[AddressKey::opaque()]), Ok(vec![zats(1)]));
        writer.stop().await.expect("stops at Shutdown");

        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        let resumed = reader(&writer).balances(&[bob]);
        assert_eq!(resumed, Ok(vec![zats(560)]), "resumed at 2, no replay");
        for block in &blocks[1..3] {
            writer.send(step(block)).await;
        }
        writer.send(finalized(&blocks[3])).await;
        writer.durable_at(h(3)).await;
        let at_three = reader(&writer);
        let balances = at_three.balances(&[alice, bob]);
        assert_eq!(balances, Ok(vec![zats(290), zats(560)]), "resent 2 not applied twice");
        let unspent =
            AddressUtxo { outpoint: outpoint([0x30; 32], 0), height: h(3), value: zats(290) };
        let recent = at_three.utxos_of(&[alice], h(3));
        assert_eq!(recent, Ok(vec![vec![unspent]]), "start filters the reply, not spends");
        writer.stop().await.expect("stops at Shutdown");
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
            moves in moves(),
        ) {
            block_on_paused(random_history(plans, moves));
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
                    None => utxos.push(AddressUtxo {
                        outpoint: OutPoint { txid, vout },
                        height: h(received),
                        value: Zatoshis::from_u64(zats).expect("in supply"),
                    }),
                }
            }
            touched.sort_unstable();
            touched.dedup();
            let balance = utxos.iter().map(|utxo| utxo.value.into_u64()).sum();
            (utxos, Zatoshis::from_u64(balance).expect("in supply"), touched)
        };

        let fs = SimFs::new();
        let mut stream = FinalStream::new(&blocks);
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
        for (at, next) in moves.into_iter().enumerate() {
            if next == Move::Reopen {
                writer.stop().await.expect("stops at Shutdown");
                writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
            }
            for step in stream.steps(next) {
                writer.send(step).await;
            }
            writer.durable_at(stream.tip()).await;

            let held = stream.sent() as u32;
            let reader = TransparentAddressReader::new(writer.handle().view());
            if held > 0 {
                for tag in 0..3u8 {
                    let served = reader.transactions(&address(tag), h(0), h(held - 1));
                    let touched = expected(tag, held).2;
                    assert_eq!(served, Ok(touched), "move {at} {next:?}: transactions of {tag}");
                }
            }

            // every address in one request, repeats included: one batched spend lookup, each
            // answer its own address's
            let tags = [2u8, 0, 1, 0];
            let addresses = tags.map(address);
            let utxos = tags.map(|tag| expected(tag, held).0).to_vec();
            let served = reader.utxos_of(&addresses, h(0));
            assert_eq!(served, Ok(utxos), "move {at} {next:?}: utxos of all");
            let balances = tags.map(|tag| expected(tag, held).1).to_vec();
            let served = reader.balances(&addresses);
            assert_eq!(served, Ok(balances), "move {at} {next:?}: balances of all");
        }
        writer.stop().await.expect("stops at Shutdown");
    }
}
