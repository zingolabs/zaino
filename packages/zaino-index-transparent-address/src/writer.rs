//! transparent_address writer: the finalized stream → one [`fold`] per block → its store

use std::time::{Duration, Instant};

use zaino_persistence::{BlockChanges, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Subscription,
};

use crate::{
    address::AddressKey,
    layout::{encode_receive, encode_spend, encode_spent_key, ReceiveKey, ReceiveRow, Spend},
    TransparentAddressReader, RECEIVES, SPENT,
};

pub struct TransparentAddressIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: MapRead>> TransparentAddressIndexWriter<S> {
    pub fn new(store: S) -> Self {
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    ///
    /// - commits after the finalized tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                for block in &run.blocks {
                    if !held(&store, block) {
                        let mut changes = store.changes(block.block_ref());
                        fold(&TransparentAddressReader::new(store.applied()), block, &mut changes);
                        write += apply(&mut store, changes);
                    }
                }
                if run.ends_at_finalized_tip {
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
    let height = block.header().height;
    for tx in block.transactions() {
        // coinbase inputs elided upstream (`zaino-source` decode.rs)
        let mut spent = out.map(SPENT);
        for input in &tx.transparent.inputs {
            let key = encode_spent_key(input.txid(), input.n());
            spent.insert(&key, &encode_spend(&Spend { height, spender: tx.txid }));
        }
        let mut receives = out.map(RECEIVES);
        for (vout, output) in (0u32..).zip(&tx.transparent.outputs) {
            let address = AddressKey::of_script(&output.script.0 .0);
            let key = ReceiveKey { address, height, txid: tx.txid, vout };
            let (key, value) = encode_receive(&ReceiveRow { key, value: output.value });
            receives.insert(&key, &value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use proptest::collection::vec;
    use proptest::prelude::{prop_assert_eq, Strategy, TestCaseError};
    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_source::testing::{h, p2pkh, MockChain};
    use zaino_sync::testing::{
        block_on_paused, moves, open, step, write_buffers, FinalizedStreamFixture, Move, Writer,
        QUEUE,
    };
    use zcash_protocol::consensus::{BlockHeight, NetworkType, H0};
    use zcash_protocol::value::Zatoshis;
    use zcash_script::script::Code;
    use zcash_transparent::address::{Script, TransparentAddress};
    use zcash_transparent::bundle::OutPoint;

    use super::*;
    use crate::{address::AddressKey, AddressUtxo, ServeError};

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = TransparentAddressIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
    }

    /// Genesis pays alice 500, bob 70, an opaque script 1; block 1 spends alice's, pays bob 490
    ///
    /// - its `BlockChanges` = spend under the outpoint + bob's receive (nothing looked up)
    /// - read over parent + it: alice's receive spent by block 1, bob's two unspent, opaque kept
    #[test]
    fn a_spend_folded_on_its_parent_retires_the_receive_the_parent_holds() {
        let zats = |n: u64| Zatoshis::from_u64(n).expect("in supply");
        let opaque = Script(Code(vec![0x6a]));
        let mut chain = MockChain::regtest().genesis_with(|b| {
            b.coinbase(|c| {
                c.pay(&p2pkh([0xa1; 20]), 500).pay(&p2pkh([0xb0; 20]), 70).pay(&opaque, 1)
            })
        });
        let paid = chain.output(chain.genesis(), 0, 0);
        let one = chain.mine(|b| b.tx(|t| t.spend(paid.clone()).pay(&p2pkh([0xb0; 20]), 490)));
        let blocks = chain.blocks(one);
        let (alice, bob) = (AddressKey::p2pkh([0xa1; 20]), AddressKey::p2pkh([0xb0; 20]));
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut genesis = store.changes(blocks[0].block_ref());
        fold(&TransparentAddressReader::new(store.applied()), &blocks[0], &mut genesis);
        store.apply(genesis);

        let mut changes = store.changes(blocks[1].block_ref());
        fold(&TransparentAddressReader::new(store.applied()), &blocks[1], &mut changes);

        let (coinbase, spender) = (chain.tx(chain.genesis(), 0).txid, chain.tx(one, 1).txid);
        let spent: Vec<_> = changes.inserts(SPENT).map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        let spend = encode_spend(&Spend { height: h(1), spender });
        let spent_key = encode_spent_key(paid.txid(), paid.n());
        assert_eq!(spent, vec![(spent_key.to_vec(), spend.to_vec())], "spent: alice's outpoint");
        let bob_receive = ReceiveRow {
            key: ReceiveKey { address: bob, height: h(1), txid: spender, vout: 0 },
            value: zats(490),
        };
        let (key, value) = encode_receive(&bob_receive);
        let receives: Vec<_> = changes.inserts(RECEIVES).collect();
        assert_eq!(receives, vec![(&key[..], &value[..])], "receives: bob's 490 only");

        store.apply(changes);
        let read = TransparentAddressReader::new(store.applied());
        let unspent = read.unspent(&[alice, bob, AddressKey::opaque()], H0, usize::MAX);
        let at_genesis = |address, vout, value| ReceiveRow {
            key: ReceiveKey { address, height: H0, txid: coinbase, vout },
            value: zats(value),
        };
        let expected = vec![
            vec![],
            vec![at_genesis(bob, 1, 70), bob_receive],
            vec![at_genesis(AddressKey::opaque(), 2, 1)],
        ];
        assert_eq!(unspent, Some(expected), "unspent of [alice, bob, opaque]");
        let alice_receive = at_genesis(alice, 0, 500).key;
        let spends = read.spends_of(&[alice_receive]);
        assert_eq!(
            spends,
            vec![Some(Spend { height: h(1), spender })],
            "alice's receive: spent by 1"
        );
    }

    /// Ten one-block commits, crashed at every persistence point: each state reopens to a durable
    /// prefix with its one unspent output and balance exact, takes the next block
    ///
    /// - write_buffer = 1 byte (each block commits as it arrives)
    /// - block `h`'s coinbase pays alice `h + 1` zats (vout 0), its tx spends block `h - 1`'s
    #[tokio::test]
    async fn every_crash_state_of_one_block_commits_reopens_to_a_durable_prefix() {
        let alice = TransparentAddress::PublicKeyHash([0xa1; 20]);
        let pays_alice = p2pkh([0xa1; 20]);
        let mut chain =
            MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&pays_alice, 1)));
        for height in 1u8..=10 {
            let paid = u64::from(height) + 1;
            let previous = chain.output(chain.tip(), 0, 0);
            chain.mine(|b| b.coinbase(|c| c.pay(&pays_alice, paid)).tx(|t| t.spend(previous)));
        }
        let blocks = chain.blocks(chain.tip());
        // `count` blocks held: alice's one unspent = block `count - 1`'s coinbase, worth `count`
        let unspent_after = |count: u32| -> Vec<AddressUtxo> {
            let Some(last) = count.checked_sub(1) else { return Vec::new() };
            let value = Zatoshis::from_u64(u64::from(count)).expect("in supply");
            vec![AddressUtxo {
                outpoint: chain.output(chain.at(h(last)), 0, 0),
                height: h(last),
                value,
            }]
        };
        let (fs, recording) = SimFs::recording();
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
        for (acked, block) in (1u64..).zip(&blocks[..10]) {
            writer.send(step(block)).await;
            writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        writer.stop().await.expect("stops at Shutdown");

        let states = recording.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MAX);
            // one block per commit: blocks held = commits recovered
            let count = store.durable().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let label = format!("{}: recovered {count} blocks, next height {count}", state.label);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(10));
            assert!(acked.contains(&u64::from(count)), "{label}: acknowledged {acked:?}");
            let reader = TransparentAddressReader::new(store.durable());
            let balance = Zatoshis::from_u64(u64::from(count)).expect("in supply");
            let utxos = reader.utxos_of(&[alice], h(0));
            assert_eq!(utxos, Ok(vec![unspent_after(count)]), "{label}: alice's utxos");
            assert_eq!(reader.balances(&[alice]), Ok(vec![balance]), "{label}: alice's balance");

            let writer = start(store);
            writer.send(step(chain.block(chain.at(h(count)).hash))).await;
            writer.stop().await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let after = TransparentAddressReader::new(
                open(&state.fs, &SCHEMA, NonZeroUsize::MAX).durable(),
            );
            let utxos = after.utxos_of(&[alice], h(0));
            assert_eq!(utxos, Ok(vec![unspent_after(count + 1)]), "{label}: utxos after next");
        }
    }

    /// Tracked addresses (p2pkh hashes), indexed by `Pay::owner`
    const OWNERS: [[u8; 20]; 3] = [[0xa0; 20], [0xa1; 20], [0xa2; 20]];
    /// Untracked p2pkh: genesis funds it, every planned tx spends it + pays the change back (vout
    /// last)
    const FAUCET: [u8; 20] = [0xff; 20];
    const FAUCET_FUNDS: u64 = 1_000_000_000;

    #[derive(Debug, Clone, Copy)]
    struct Pay {
        owner: usize,
        zats: u64,
    }

    /// One block's tx after genesis
    ///
    /// - `spends[i]` = the `spends[i]`-th output unspent before this block (oldest first, past the
    ///   end = no spend)
    #[derive(Debug, Clone)]
    struct BlockPlan {
        pays: Vec<Pay>,
        spends: Vec<usize>,
    }

    fn block_plans() -> impl Strategy<Value = Vec<BlockPlan>> {
        let pay = (0..OWNERS.len(), 1u64..1_000).prop_map(|(owner, zats)| Pay { owner, zats });
        let plan = (vec(pay, 0..3), vec(0usize..4, 0..3))
            .prop_map(|(pays, spends)| BlockPlan { pays, spends });
        vec(plan, 1..10)
    }

    /// Naive UTXO set row: one tracked output as mined
    #[derive(Debug)]
    struct Output {
        owner: usize,
        outpoint: OutPoint,
        zats: u64,
        received: (BlockHeight, zcash_protocol::TxId),
        spent: Option<(BlockHeight, zcash_protocol::TxId)>,
    }

    /// One address's answers
    #[derive(Debug, PartialEq)]
    struct Answers {
        utxos: Vec<AddressUtxo>,
        balance: Zatoshis,
        transactions: Vec<zcash_protocol::TxId>,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random spend graphs through random finalized streams (runs, restarts, write buffers):
        /// once every move commits, each address's utxos, balance and transactions equal a naive
        /// UTXO set's
        #[test]
        fn random_histories_answer_like_a_naive_utxo_set(
            plans in block_plans(),
            moves in moves(),
            write_buffer in write_buffers(),
        ) {
            block_on_paused(random_history(plans, moves, write_buffer))?;
        }
    }

    async fn random_history(
        plans: Vec<BlockPlan>,
        moves: Vec<Move>,
        write_buffer: NonZeroUsize,
    ) -> Result<(), TestCaseError> {
        // chain + naive UTXO set, built together
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.pay(&p2pkh(FAUCET), FAUCET_FUNDS)));
        let mut faucet = (chain.output(chain.genesis(), 0, 0), FAUCET_FUNDS);
        let mut outputs: Vec<Output> = Vec::new();
        for (height, plan) in (1u32..).zip(&plans) {
            let unspent: Vec<usize> =
                (0..outputs.len()).filter(|&at| outputs[at].spent.is_none()).collect();
            let mut spent: Vec<usize> =
                plan.spends.iter().filter_map(|&nth| unspent.get(nth).copied()).collect();
            spent.sort_unstable();
            spent.dedup();
            let inputs = faucet.1 + spent.iter().map(|&at| outputs[at].zats).sum::<u64>();
            let change = inputs - plan.pays.iter().map(|pay| pay.zats).sum::<u64>();
            let mined = chain.mine(|b| {
                b.tx(|t| {
                    let t = t.spend(faucet.0.clone());
                    let t = spent.iter().fold(t, |t, &at| t.spend(outputs[at].outpoint.clone()));
                    let t = plan
                        .pays
                        .iter()
                        .fold(t, |t, pay| t.pay(&p2pkh(OWNERS[pay.owner]), pay.zats));
                    t.pay(&p2pkh(FAUCET), change)
                })
            });
            let tx = (h(height), chain.tx(mined, 1).txid);
            for &at in &spent {
                outputs[at].spent = Some(tx);
            }
            for (vout, pay) in (0u32..).zip(&plan.pays) {
                let outpoint = chain.output(mined, 1, vout);
                outputs.push(Output {
                    owner: pay.owner,
                    outpoint,
                    zats: pay.zats,
                    received: tx,
                    spent: None,
                });
            }
            let change_vout =
                u32::try_from(plan.pays.len()).expect("block_plans: pays << u32::MAX");
            faucet = (chain.output(mined, 1, change_vout), change);
        }
        let blocks = chain.blocks(chain.tip());
        // naive answers once heights `0..=tip` are indexed
        let expected = |owner: usize, tip: Option<BlockHeight>| {
            let visible = |&(height, _): &(BlockHeight, zcash_protocol::TxId)| Some(height) <= tip;
            let mine =
                outputs.iter().filter(|output| output.owner == owner && visible(&output.received));
            let mut utxos = Vec::new();
            let mut transactions = Vec::new();
            for output in mine {
                transactions.push(output.received);
                match output.spent.filter(visible) {
                    Some(spender) => transactions.push(spender),
                    None => utxos.push(AddressUtxo {
                        outpoint: output.outpoint.clone(),
                        height: output.received.0,
                        value: Zatoshis::from_u64(output.zats).expect("in supply"),
                    }),
                }
            }
            transactions.sort_unstable();
            transactions.dedup();
            let balance = utxos.iter().map(|utxo| utxo.value.into_u64()).sum();
            Answers {
                utxos,
                balance: Zatoshis::from_u64(balance).expect("in supply"),
                transactions: transactions.into_iter().map(|(_, txid)| txid).collect(),
            }
        };

        let fs = SimFs::new();
        let mut stream = FinalizedStreamFixture::new(&blocks);
        let mut writer = start(open(&fs, &SCHEMA, write_buffer));
        for (at, next) in moves.into_iter().enumerate() {
            if next == Move::Reopen {
                writer.stop().await.expect("stops at Shutdown");
                writer = start(open(&fs, &SCHEMA, write_buffer));
            }
            for step in stream.steps(next) {
                writer.send(step).await;
            }
            writer.durable_at(stream.tip()).await;

            let tip = stream.tip();
            let reader = TransparentAddressReader::new(writer.handle().durable_view());
            let refused = |method: &str| {
                let method = method.to_owned();
                move |error: ServeError| {
                    TestCaseError::fail(format!("move {at} {next:?}: {method}: {error}"))
                }
            };
            // every owner in one request, a repeat included: one batched spend lookup, each answer
            // its own address's
            let owners = [2usize, 0, 1, 0];
            let addresses = owners.map(|owner| TransparentAddress::PublicKeyHash(OWNERS[owner]));
            let utxos = reader.utxos_of(&addresses, h(0)).map_err(refused("utxos_of"))?;
            let balances = reader.balances(&addresses).map_err(refused("balances"))?;
            for (slot, ((owner, utxos), balance)) in
                owners.into_iter().zip(utxos).zip(balances).enumerate()
            {
                let transactions = match tip {
                    Some(tip) => reader
                        .transactions(&addresses[slot], h(0), tip)
                        .map_err(refused("transactions"))?,
                    None => Vec::new(),
                };
                let served = Answers { utxos, balance, transactions };
                prop_assert_eq!(
                    served,
                    expected(owner, tip),
                    "served vs naive UTXO set, move {} {:?}: owner {} (slot {}), tip {:?}",
                    at,
                    next,
                    owner,
                    slot,
                    tip
                );
            }
        }
        writer.stop().await.expect("stops at Shutdown");
        Ok(())
    }
}
