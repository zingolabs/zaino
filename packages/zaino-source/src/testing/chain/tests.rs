use zaino_primitives::sha256d;
use zcash_primitives::transaction::{CompressedTransaction, TxVersion};
use zcash_protocol::consensus::{BranchId, NetworkUpgrade};
use zcash_protocol::value::{Zatoshis, MAX_MONEY};
use zcash_protocol::ShieldedPool;

use super::*;
use crate::testing::{commitment, nullifier, p2pkh};
use zaino_primitives::types::TransparentOutput;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// M1–M4 on every block handed out, and what reads them back
///
/// - genesis pinned byte for byte, each hash = SHA-256d of its header bytes
/// - every block = the decode of its own bytes
/// - every txid = the hash of its bytes
/// - each child one height and one target spacing up (150 s pre-Blossom, 75 s, 25 s from NU7)
/// - merkle root = the txids', one coinbase in slot 0
/// - siblings distinct, the first mined = best
#[test]
fn every_block_is_the_decode_of_its_own_real_bytes() {
    let mut chain = MockChain::regtest();
    let genesis = chain.genesis();
    let golden = [
        "04000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "8fd4dec6203afc91016e177642dc163ecb0dfe49f4182040be3c717428ddaee4",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "00f15365",
        "0f0f0f20",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "24000000000000000000000000000000000000000000000000000000000000000000000000",
    ];
    assert_eq!(hex(&chain.header_bytes(genesis.hash)), golden.concat());

    let one = chain.mine_empty(1);
    let sibling = chain.branch(genesis).mine_empty(1).tip();
    assert_ne!(one.hash, sibling.hash, "same parent, same time: distinct blocks");
    assert_eq!(chain.tip(), one, "a tie keeps the first mined");
    let alice = p2pkh([0xaa; 20]);
    let two = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)).tx(|t| t.sapling_output(1)));
    assert_eq!((chain.tip(), chain.at(h(1)), chain.at(h(0))), (two, one, genesis));

    let blocks = chain.blocks(two);
    let hashes: Vec<BlockHash> = blocks.iter().map(|block| block.header().hash).collect();
    assert_eq!(hashes, [genesis.hash, one.hash, two.hash]);
    for block in &blocks {
        let decoded = crate::decode_block(chain.block_bytes(block.header().hash)).expect("decodes");
        assert_eq!(format!("{decoded:?}"), format!("{block:?}"), "held = decoded bytes");
        for tx in block.transactions() {
            let bytes = chain.tx_bytes(tx.txid).expect("every minted tx keeps its bytes");
            assert_eq!(crate::decode_transaction(bytes).expect("decodes").txid, tx.txid);
        }
    }
    for (parent, block) in blocks.iter().zip(&blocks[1..]) {
        let header = block.header();
        let txids: Vec<TxId> = block.transactions().iter().map(|tx| tx.txid).collect();
        let coinbases: Vec<bool> =
            block.transactions().iter().map(|tx| tx.transparent.coinbase).collect();
        assert_eq!(sha256d(&chain.header_bytes(header.hash)), header.hash.0);
        assert_eq!(
            (header.prev_hash, header.height),
            (parent.header().hash, parent.header().height + 1)
        );
        assert_eq!(header.time, parent.header().time + 75);
        assert_eq!(Some(header.merkle_root), MerkleRoot::of_txids(&txids));
        let slot_zero: Vec<bool> = (0..coinbases.len()).map(|slot| slot == 0).collect();
        assert_eq!(coinbases, slot_zero, "one coinbase, slot 0");
    }
    assert_eq!(chain.tx(two, 0).transparent.outputs[0].script, alice);
    let funded = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&alice, 7_000)));
    let output =
        TransparentOutput { value: Zatoshis::from_u64(7_000).expect("in supply"), script: alice };
    let coinbase = funded.tx(funded.genesis(), 0);
    assert_eq!(coinbase.transparent.outputs, [output], "genesis_with: genesis as stated");
    assert_ne!(funded.genesis(), genesis);

    let early = Some(h(1));
    let upgrades =
        LocalNetwork { overwinter: early, sapling: early, nu7: Some(h(5)), ..all_at(h(3)) };
    let mut scheduled = MockChain::regtest().upgrades(upgrades);
    let six = scheduled.mine_empty(6);
    let times: Vec<u32> = scheduled.blocks(six).iter().map(|block| block.header().time).collect();
    let spacing: Vec<u32> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(spacing, [150, 150, 75, 75, 25, 25], "pre-Blossom, Blossom at 3, NU7 at 5");
}

/// Each transaction in the format its height's upgrade writes, v5+ embedding that branch
///
/// - v1 before Overwinter, v3, v4 from Sapling, v5 from NU5
/// - Sprout forces v4, Ironwood v6
#[test]
fn each_transaction_is_written_in_its_heights_format() {
    let upgrades = LocalNetwork::from_activation_heights(|upgrade| match upgrade {
        NetworkUpgrade::Overwinter => Some(h(1)),
        NetworkUpgrade::Sapling => Some(h(2)),
        NetworkUpgrade::Nu7 => None,
        NetworkUpgrade::Nu6_3 => Some(h(4)),
        _ => Some(h(3)),
    });
    let mut chain = MockChain::regtest().upgrades(upgrades);
    chain.mine_empty(3);
    let alice = p2pkh([0xaa; 20]);
    let four = chain.mine(|b| {
        b.coinbase(|c| c.pay(&alice, 5))
            .tx(|t| t.ironwood_action(1, 1))
            .tx(|t| t.spend_in_block(0, 0).sprout_balance(-5))
    });
    let format = |at: BlockRef, slot: usize| {
        let bytes = chain.tx_bytes(chain.tx(at, slot).txid).expect("minted");
        let tx =
            CompressedTransaction::read(bytes, BranchId::Sprout).expect("librustzcash reads it");
        let embeds = matches!(tx.version(), TxVersion::V5 | TxVersion::V6);
        (tx.version(), embeds.then(|| tx.consensus_branch_id()))
    };

    let coinbases: Vec<_> = (0..=4).map(|height| format(chain.at(h(height)), 0)).collect();
    assert_eq!(
        coinbases,
        [
            (TxVersion::Sprout(1), None),
            (TxVersion::V3, None),
            (TxVersion::V4, None),
            (TxVersion::V5, Some(BranchId::Nu6_2)),
            (TxVersion::V5, Some(BranchId::Nu6_3)),
        ]
    );
    assert_eq!(format(four, 1), (TxVersion::V6, Some(BranchId::Nu6_3)), "Ironwood: v6");
    assert_eq!(format(four, 2), (TxVersion::V4, None), "Sprout: v4");
}

/// M6 + M7 accepted: Alice's 50 000 coinbase funds a block whose txs spend in order, one of them
/// an output earlier in the same block
///
/// - Sapling takes 29 000 in and gives 20 000 back, Sprout 18 000 and 9 000
/// - stated fees match, unstated ones derived, `fees` = `[Coinbase, Paid(..)..]`
/// - note tags land as their canonical nullifiers and commitments
/// - a sibling branch may mine the same transaction (a reorged tx, re-mined from its bytes)
#[test]
fn spends_move_value_along_their_own_branch_and_each_fee_is_what_conservation_leaves() {
    let (alice, bob) = (p2pkh([0xaa; 20]), p2pkh([0xbb; 20]));
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let spent = chain.mine(|b| {
        b.tx(|t| t.spend(funding.clone()).pay(&bob, 30_000).pay(&alice, 19_000).fee(1_000))
            .tx(|t| t.spend_in_block(1, 1).pay(&bob, 18_500))
            .tx(|t| {
                t.spend_in_block(1, 0)
                    .sapling_output(7)
                    .orchard_action(4, 9)
                    .value_balance(ShieldedPool::Sapling, -29_000)
            })
    });
    let change = chain.output(spent, 2, 0);
    let withdrawn = chain.mine(|b| {
        b.tx(|t| {
            t.sapling_spend(5)
                .value_balance(ShieldedPool::Sapling, 20_000)
                .pay(&alice, 19_900)
                .fee(100)
        })
        .tx(|t| t.spend(change).sprout_balance(-18_000).fee(500))
        .tx(|t| t.sprout_balance(9_000).pay(&bob, 8_000))
    });
    let paid = |zats: u64| Fee::Paid(Zatoshis::from_u64(zats).expect("in supply"));
    assert_eq!(chain.fees(funded.hash).fees, [Fee::Coinbase]);
    assert_eq!(chain.fees(spent.hash).fees, [Fee::Coinbase, paid(1_000), paid(500), paid(1_000)]);
    let fees = vec![Fee::Coinbase, paid(100), paid(500), paid(1_000)];
    assert_eq!(chain.fees(withdrawn.hash), BlockFees { height: h(3), hash: withdrawn.hash, fees });

    let shielded = chain.tx(spent, 3);
    assert_eq!(shielded.sapling.outputs[0].cmu, commitment(7));
    assert_eq!(shielded.orchard.actions[0].cmx, commitment(9));
    assert_eq!(shielded.orchard.actions[0].nullifier, nullifier(4));
    assert_eq!(chain.tx(withdrawn, 1).sapling.spends[0].nullifier, nullifier(5));
    let keys =
        [shielded.sapling.outputs[0].ephemeral_key, shielded.orchard.actions[0].ephemeral_key];
    assert_ne!(keys[0], keys[1], "detection material distinct per pool");
    assert_eq!(chain.tx(spent, 1).transparent.inputs, [funding]);
    let in_block = OutPoint::new(chain.tx(spent, 1).txid.into(), 1);
    assert_eq!(chain.tx(spent, 2).transparent.inputs, [in_block], "spend_in_block(1, 1)");

    let resent = chain.tx_bytes(chain.tx(spent, 1).txid).expect("minted").to_vec();
    let remined = chain.branch(funded).mine(|b| b.raw_tx(resent)).tip();
    assert_eq!(chain.fees(remined.hash).fees, [Fee::Coinbase, paid(1_000)]);
    assert_eq!(chain.tx(remined, 1).txid, chain.tx(spent, 1).txid, "same bytes, same txid");
    assert_eq!(chain.tip(), withdrawn, "a shorter sibling stays a side branch");
}

// constructions no real chain holds: each panics naming the rule it breaks

#[test]
#[should_panic(expected = "not unspent on this branch")]
fn spending_an_output_never_mined_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.spend(OutPoint::new([0x99; 32], 0))));
}

#[test]
#[should_panic(expected = "not unspent on this branch")]
fn spending_an_output_a_block_before_spent_panics() {
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&p2pkh([0xaa; 20]), 50_000)));
    let funding = chain.output(funded, 0, 0);
    chain.mine(|b| b.tx(|t| t.spend(funding.clone())));
    chain.mine(|b| b.tx(|t| t.spend(funding)));
}

#[test]
#[should_panic(expected = "not unspent on this branch")]
fn spending_an_output_twice_in_one_block_panics() {
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&p2pkh([0xaa; 20]), 50_000)));
    let funding = chain.output(funded, 0, 0);
    chain.mine(|b| b.tx(|t| t.spend(funding.clone())).tx(|t| t.spend(funding)));
}

#[test]
#[should_panic(expected = "not unspent on this branch")]
fn spending_an_output_off_its_branch_panics() {
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&p2pkh([0xaa; 20]), 50_000)));
    let funding = chain.output(funded, 0, 0);
    let genesis = chain.genesis();
    chain.branch(genesis).mine(|b| b.tx(|t| t.spend(funding)));
}

#[test]
#[should_panic(expected = "slot 1 spends slot 2: not minted before it")]
fn spending_a_later_slot_of_the_same_block_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.spend_in_block(2, 0)).tx(|t| t.pay(&p2pkh([0xaa; 20]), 1)));
}

#[test]
#[should_panic(expected = "overspends by 1")]
fn paying_past_the_inputs_panics() {
    let (alice, mut chain) = (p2pkh([0xaa; 20]), MockChain::regtest());
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    chain.mine(|b| b.tx(|t| t.spend(funding).pay(&alice, 50_001)));
}

#[test]
#[should_panic(expected = "stated fee 500 != inputs − outputs + balances (1000)")]
fn a_stated_fee_off_the_derived_one_panics() {
    let (alice, mut chain) = (p2pkh([0xaa; 20]), MockChain::regtest());
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    chain.mine(|b| b.tx(|t| t.spend(funding).pay(&alice, 49_000).fee(500)));
}

#[test]
#[should_panic(expected = "txid repeated on this branch")]
fn a_transaction_mined_twice_on_its_branch_panics() {
    let mut chain = MockChain::regtest();
    let one = chain.mine(|b| b.tx(|t| t.sapling_output(1)));
    let again = chain.tx_bytes(chain.tx(one, 1).txid).expect("minted").to_vec();
    chain.mine(|b| b.raw_tx(again));
}

#[test]
#[should_panic(expected = "pool data before Sapling")]
fn sapling_data_before_sapling_panics() {
    let mut chain = MockChain::regtest().upgrades(all_at(h(5)));
    chain.mine(|b| b.tx(|t| t.sapling_output(1)));
}

#[test]
#[should_panic(expected = "pool data before Nu5")]
fn orchard_data_before_nu5_panics() {
    let upgrades = LocalNetwork::from_activation_heights(|upgrade| match upgrade {
        NetworkUpgrade::Nu7 => None,
        NetworkUpgrade::Nu5 | NetworkUpgrade::Nu6 | NetworkUpgrade::Nu6_1 => Some(h(3)),
        NetworkUpgrade::Nu6_2 | NetworkUpgrade::Nu6_3 => Some(h(3)),
        _ => Some(h(1)),
    });
    let mut chain = MockChain::regtest().upgrades(upgrades);
    chain.mine(|b| b.coinbase(|c| c.orchard_action(1, 1)));
}

#[test]
#[should_panic(expected = "pool data before Nu6.3")]
fn ironwood_data_before_nu6_3_panics() {
    let upgrades = LocalNetwork { nu6_3: None, ..all_at(h(1)) };
    let mut chain = MockChain::regtest().upgrades(upgrades);
    chain.mine(|b| b.tx(|t| t.ironwood_action(1, 1)));
}

#[test]
#[should_panic(expected = "value_balance(Sapling) with no Sapling spend or output")]
fn a_balance_without_its_pools_description_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.value_balance(ShieldedPool::Sapling, -1)));
}

#[test]
#[should_panic(expected = "nullifier revealed twice")]
fn a_sapling_nullifier_revealed_twice_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.sapling_spend(5)));
    chain.mine(|b| b.tx(|t| t.sapling_spend(5)));
}

#[test]
#[should_panic(expected = "Orchard nullifier revealed twice")]
fn an_orchard_nullifier_revealed_twice_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.orchard_action(6, 1)));
    chain.mine(|b| b.coinbase(|c| c.orchard_action(6, 2)));
}

#[test]
#[should_panic(expected = "ZIP 209")]
fn a_pool_driven_below_zero_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.sapling_spend(5).value_balance(ShieldedPool::Sapling, 1)));
}

#[test]
#[should_panic(expected = "a coinbase spends nothing")]
fn a_coinbase_spending_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.spend(OutPoint::new([0x10; 32], 0))));
}

#[test]
#[should_panic(expected = "a coinbase pays no fee")]
fn a_coinbase_paying_a_fee_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.fee(1)));
}

#[test]
#[should_panic(expected = "past the supply")]
fn a_coinbase_past_the_supply_panics() {
    let past_supply = MAX_MONEY + 1;
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.pay(&p2pkh([0xaa; 20]), past_supply)));
}

#[test]
#[should_panic(expected = "not after the median time past")]
fn a_time_at_or_before_the_median_time_past_panics() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.time(GENESIS_TIME));
}

#[test]
#[should_panic(expected = "bits() under Work::Limit")]
fn stated_bits_under_limit_work_panic() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.bits(REGTEST_BITS));
}

#[test]
#[should_panic(expected = "outweigh() under Work::Limit")]
fn outweighing_under_limit_work_panics() {
    let mut chain = MockChain::regtest();
    chain.fork(h(0)).outweigh();
}

#[test]
#[should_panic(expected = "bits() on an outweigh() block")]
fn stated_bits_on_an_outweighing_block_panic() {
    let mut chain = MockChain::regtest().varied_work();
    chain.fork(h(0)).outweigh().mine(|b| b.bits(REGTEST_BITS));
}

#[test]
#[should_panic(expected = "configured after its first mine")]
fn configuring_after_the_first_mine_panics() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(1);
    chain.varied_work();
}

#[test]
#[should_panic(expected = "Sapling activates at 5, below Overwinter at 6")]
fn upgrades_out_of_order_panic() {
    let upgrades = LocalNetwork { sapling: Some(h(5)), ..all_at(h(6)) };
    MockChain::regtest().upgrades(upgrades);
}

#[test]
#[should_panic(expected = "above the best tip")]
fn a_height_above_the_best_tip_panics() {
    MockChain::regtest().at(h(1));
}

/// M5: `outweigh()` = the best right after its first block, by the least work that wins
///
/// - retreat (12 → 11), same-height replacement, revival of an old tip
/// - later blocks at the limit
/// - one nBits step easier would have lost
/// - side branch never moves the tip
/// - `Work::Limit`: every nBits the limit, the longer fork wins, an equal one does not
#[test]
fn outweigh_takes_the_best_with_the_least_work_that_wins() {
    let work = |chain: &MockChain, tip: BlockRef| -> u128 {
        chain.blocks(tip).iter().map(|block| block.header().bits.work()).sum()
    };
    let mut chain = MockChain::regtest().varied_work();
    let twelve = chain.mine_empty(12);

    let retreat = chain.fork(h(10)).outweigh().mine_empty(1).tip();
    assert_eq!((chain.tip(), retreat.height), (retreat, h(11)), "retreat: 12 → 11");
    assert!(work(&chain, retreat) > work(&chain, twelve));
    let bits = chain.block(retreat.hash).header().bits;
    assert!(bits.bits() & 0xff_ffff < 0x7f_ffff, "a next-easier mantissa exists");
    let easier = CompactDifficulty::try_from_bits(bits.bits() + 1).expect("valid nBits");
    let with_easier = work(&chain, retreat) - bits.work() + easier.work();
    assert!(with_easier <= work(&chain, twelve), "one step easier loses: minimal margin");

    let replaced = chain.fork(h(10)).outweigh().mine_empty(1).tip();
    assert_eq!((chain.tip(), replaced.height), (replaced, h(11)), "same-height replacement");
    let revived = chain.branch(twelve).outweigh().mine_empty(3).tip();
    assert_eq!((chain.tip(), chain.at(h(12))), (revived, twelve), "old tip revived");
    let above: Vec<u32> =
        chain.blocks(revived)[14..].iter().map(|b| b.header().bits.bits()).collect();
    assert_eq!(above, [REGTEST_BITS; 2], "only the first block outweighs");
    let side = chain.fork(h(5)).mine_empty(2).tip();
    assert_eq!((chain.tip(), side.height), (revived, h(7)), "a side branch is not best");
    let stated = chain.mine(|b| b.bits(0x2007_ffff));
    assert_eq!(chain.block(stated.hash).header().bits.bits(), 0x2007_ffff, "bits() as stated");

    let mut limit = MockChain::regtest();
    let three = limit.mine_empty(3);
    let equal = limit.fork(h(1)).mine_empty(2).tip();
    assert_eq!(limit.tip(), three, "equal work: the first mined stays best");
    let longer = limit.branch(equal).mine_empty(1).tip();
    assert_eq!(limit.tip(), longer, "Work::Limit: the longer fork wins");
    let bits: Vec<u32> = limit.blocks(longer).iter().map(|b| b.header().bits.bits()).collect();
    assert_eq!(bits, [REGTEST_BITS; 5]);
}

/// Mempool on Alice's 50 000 coinbase: `parent` (fee 1 000), `child` spending it (fee 500)
///
/// - each held at the fee its spends leave
/// - evicting `parent` drops `child` (orphaned)
/// - resubmitted from their bytes, `mine_mempool` carries both in order at those fees
/// - mempool then empty
#[test]
fn the_mempool_holds_what_the_best_admits_until_mined() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let held = |chain: &MockChain| -> Vec<(TxId, u64)> {
        chain.mempool().iter().map(|unmined| (unmined.tx.txid, unmined.fee.into_u64())).collect()
    };

    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    let child = chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));
    assert_eq!(held(&chain), [(parent, 1_000), (child, 500)]);
    let bytes = |txid: TxId| chain.tx_bytes(txid).expect("submitted").to_vec();
    let (parent_bytes, child_bytes) = (bytes(parent), bytes(child));
    chain.evict(parent);
    assert_eq!(held(&chain), [], "the child leaves with its parent");

    chain.submit_raw(parent_bytes);
    chain.submit_raw(child_bytes);
    let mined = chain.mine_mempool();
    let carried = chain.block(mined.hash).transactions().iter().skip(1).map(|tx| tx.txid);
    assert_eq!(carried.collect::<Vec<_>>(), [parent, child]);
    let paid = |zats: u64| Fee::Paid(Zatoshis::from_u64(zats).expect("in supply"));
    assert_eq!(chain.fees(mined.hash).fees, [Fee::Coinbase, paid(1_000), paid(500)]);
    assert_eq!(held(&chain), [], "mined = out of the mempool");
}

/// Reorgs around a block mining `parent` + `child` (Alice's coinbase at 1 funds `parent`)
///
/// - its branch outweighed: both back, block order
/// - it revived: both mined again, gone
/// - a winner spending `parent`'s prevout first: `parent` conflicts, `child` orphaned, both gone
#[test]
fn a_reorg_returns_what_the_best_no_longer_mines_and_drops_conflicts() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest().varied_work();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding.clone()).pay(&alice, 49_000));
    let child = chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));
    let mined = chain.mine_mempool();
    let held = |chain: &MockChain| -> Vec<TxId> {
        chain.mempool().iter().map(|unmined| unmined.tx.txid).collect()
    };

    chain.fork(h(1)).outweigh().mine_empty(1);
    assert_eq!(held(&chain), [parent, child], "unmined by the reorg: back, in order");
    let revived = chain.branch(mined).outweigh().mine_empty(1).tip();
    assert_eq!((chain.tip(), held(&chain)), (revived, vec![]), "mined again: gone");

    chain.fork(h(1)).outweigh().mine(|b| b.tx(|t| t.spend(funding).pay(&alice, 40_000)));
    assert_eq!(held(&chain), [], "its prevout spent by the winner: dropped, its child too");
}

/// A double spend of a held entry's prevout: never held beside it
#[test]
#[should_panic(expected = "not unspent on this branch")]
fn submitting_a_rival_to_a_held_spend_panics() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    chain.submit(|t| t.spend(funding.clone()).pay(&alice, 49_000));
    chain.submit(|t| t.spend(funding).pay(&alice, 48_000));
}
