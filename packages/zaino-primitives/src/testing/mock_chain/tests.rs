use std::panic::{catch_unwind, AssertUnwindSafe};

use zcash_protocol::consensus::{BranchId, NetworkUpgrade};

use super::*;
use crate::testing::TxBuilder;
use crate::testing::{outpoint, p2pkh};
use crate::types::{ConsensusBranchId, NetworkUpgradeStatus, NoteCommitment, TransparentOutput};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn branch(id: BranchId) -> ConsensusBranchId {
    ConsensusBranchId::new(u32::from(id))
}

/// M1–M4 on every block handed out, and what reads them back:
/// - genesis pinned byte for byte; each hash = SHA-256d of its header bytes
/// - each child one height and one target spacing up (150 s pre-Blossom, 75 s, 25 s from NU7)
/// - merkle root = the txids'; one coinbase, slot 0; siblings distinct, the first mined best
/// - `getblockchaininfo`: active / pending by the tip, branch ids at the tip and the next block
#[test]
fn every_block_is_a_real_linked_header_over_its_own_transactions() {
    let mut chain = MockChain::regtest();
    let genesis = chain.genesis();
    let golden = [
        "04000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "7ef0ca626bbb058dd443bb78e33b888bdec8295c96e51f5545f96370870c10b9",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "00f15365",
        "0f0f0f20",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "24000000000000000000000000000000000000000000000000000000000000000000000000",
    ];
    assert_eq!(hex(&chain.header_bytes(genesis.hash)), golden.concat());
    let display = "2c7c50c5b6ed3a223ec575e024891141c64d2a6469a8db045d53229e06aa7e7e";
    assert_eq!(genesis.hash.to_string(), display);

    let one = chain.mine_empty(1);
    let sibling = chain.branch(genesis).mine_empty(1).tip();
    assert_ne!(one.hash, sibling.hash, "same parent, same time: distinct blocks");
    assert_eq!(chain.tip(), one, "a tie keeps the first mined");
    let alice = p2pkh([0xaa; 20]);
    let two = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)).tx(|t| t.txid([0x22; 32])));
    assert_eq!((chain.tip(), chain.at(h(1)), chain.at(h(0))), (two, one, genesis));

    let blocks = chain.blocks(two);
    let hashes: Vec<BlockHash> = blocks.iter().map(|block| block.header().hash).collect();
    assert_eq!(hashes, [genesis.hash, one.hash, two.hash]);
    for (parent, block) in blocks.iter().zip(&blocks[1..]) {
        let header = block.header();
        let txids: Vec<TransactionId> = block.transactions().iter().map(|tx| tx.txid).collect();
        let coinbases: Vec<bool> =
            block.transactions().iter().map(|tx| tx.transparent.coinbase).collect();
        assert_eq!(sha256d(&chain.header_bytes(header.hash)), <[u8; 32]>::from(header.hash));
        assert_eq!(
            (header.prev_hash, header.height),
            (parent.header().hash, parent.header().height.next())
        );
        assert_eq!(header.time, parent.header().time + 75);
        assert_eq!(Some(header.merkle_root), MerkleRoot::of_txids(&txids));
        let slot_zero: Vec<bool> = (0..coinbases.len()).map(|slot| slot == 0).collect();
        assert_eq!(coinbases, slot_zero, "one coinbase, slot 0");
    }
    assert_eq!(blocks[2].transactions()[0].transparent.outputs[0].script, alice);
    assert_eq!(blocks[2].transactions()[1].txid, TransactionId::from([0x22; 32]));
    let funded = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&alice, 7_000)));
    let coinbase = &funded.block(funded.genesis().hash).transactions()[0];
    let output =
        TransparentOutput { value: Zatoshis::new(7_000).expect("in supply"), script: alice };
    assert_eq!(coinbase.transparent.outputs, [output], "genesis_with: genesis as stated");
    assert_ne!(funded.genesis(), genesis);

    let upgrades = Upgrades::all_at(h(3)).with(NetworkUpgrade::Overwinter, h(1));
    let upgrades = upgrades.with(NetworkUpgrade::Sapling, h(1)).with(NetworkUpgrade::Nu7, h(5));
    let mut scheduled = MockChain::regtest().upgrades(upgrades);
    let six = scheduled.mine_empty(6);
    let times: Vec<u32> = scheduled.blocks(six).iter().map(|block| block.header().time).collect();
    let spacing: Vec<u32> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(spacing, [150, 150, 75, 75, 25, 25], "pre-Blossom, Blossom at 3, NU7 at 5");
    let info = scheduled.blockchain_info(scheduled.at(h(4)));
    assert_eq!((info.blocks, info.best_block_hash), (h(4), scheduled.at(h(4)).hash));
    assert_eq!((info.estimated_height, info.sapling_activation), (h(4), h(1)));
    let statuses: Vec<(String, Height, NetworkUpgradeStatus)> = info
        .upgrades
        .iter()
        .map(|upgrade| (upgrade.name.clone(), upgrade.activation_height, upgrade.status))
        .collect();
    let (active, pending) = (NetworkUpgradeStatus::Active, NetworkUpgradeStatus::Pending);
    assert_eq!(
        statuses[..2],
        [("Overwinter".into(), h(1), active), ("Sapling".into(), h(1), active)]
    );
    assert_eq!(statuses.last(), Some(&("Nu7".into(), h(5), pending)));
    assert_eq!(statuses.len(), 11, "every scheduled upgrade, NU7 included");
    let consensus = (info.consensus.chain_tip, info.consensus.next_block);
    assert_eq!(consensus, (branch(BranchId::Nu6_3), branch(BranchId::Nu7)), "NU7 activates next");
}

/// M6 + M7 accepted: Alice's 50 000 coinbase funds a block whose txs spend in order, one of them
/// an output earlier in the same block; Sapling takes 29 000 in and gives 20 000 back, Sprout
/// 18 000 and 9 000
/// - stated fees match, unstated ones are derived; `fees` = `[Coinbase, Paid(..)..]`
/// - a sibling branch may mine the same txid (a reorged tx, re-mined)
#[test]
fn spends_move_value_along_their_own_branch_and_each_fee_is_what_conservation_leaves() {
    let (alice, bob) = (p2pkh([0xaa; 20]), p2pkh([0xbb; 20]));
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
    let spent = chain.mine(|b| {
        b.tx(|t| {
            t.txid([0x20; 32])
                .spend(outpoint([0x10; 32], 0))
                .pay(&bob, 30_000)
                .pay(&alice, 19_000)
                .fee(1_000)
        })
        .tx(|t| t.txid([0x21; 32]).spend(outpoint([0x20; 32], 1)).pay(&bob, 18_500))
        .tx(|t| {
            t.txid([0x22; 32])
                .spend(outpoint([0x20; 32], 0))
                .sapling_output(7)
                .orchard_action([0x04; 32], 9)
                .value_balance(ShieldedPool::Sapling, -29_000)
        })
    });
    let withdrawn = chain.mine(|b| {
        b.tx(|t| {
            t.sapling_spend([0x05; 32])
                .value_balance(ShieldedPool::Sapling, 20_000)
                .pay(&alice, 19_900)
                .fee(100)
        })
        .tx(|t| t.spend(outpoint([0x21; 32], 0)).sprout_balance(-18_000).fee(500))
        .tx(|t| t.sprout_balance(9_000).pay(&bob, 8_000))
    });
    let paid = |zats: u64| Fee::Paid(Zatoshis::new(zats).expect("in supply"));
    assert_eq!(chain.fees(funded.hash).fees, [Fee::Coinbase]);
    assert_eq!(chain.fees(spent.hash).fees, [Fee::Coinbase, paid(1_000), paid(500), paid(1_000)]);
    let fees = vec![Fee::Coinbase, paid(100), paid(500), paid(1_000)];
    assert_eq!(chain.fees(withdrawn.hash), BlockFees { height: h(3), hash: withdrawn.hash, fees });

    let block = chain.block(spent.hash);
    let shielded = &block.transactions()[3];
    let leaf =
        |leaf: u8| NoteCommitment::from(core::array::from_fn(|i| if i == 0 { leaf } else { 0 }));
    assert_eq!(shielded.sapling.outputs[0].cmu, leaf(7));
    assert_eq!(shielded.orchard.actions[0].cmx, leaf(9));
    assert_eq!(shielded.orchard.actions[0].nullifier, Nullifier::from([0x04; 32]));
    let keys =
        [shielded.sapling.outputs[0].ephemeral_key, shielded.orchard.actions[0].ephemeral_key];
    assert_ne!(keys[0], keys[1], "detection material distinct per pool");
    assert_eq!(block.transactions()[1].transparent.inputs, [outpoint([0x10; 32], 0)]);

    let resent = |t: TxBuilder| t.txid([0x20; 32]).spend(outpoint([0x10; 32], 0)).pay(&bob, 49_000);
    let remined = chain.branch(funded).mine(|b| b.tx(resent)).tip();
    assert_eq!(chain.fees(remined.hash).fees, [Fee::Coinbase, paid(1_000)]);
    assert_eq!(chain.tip(), withdrawn, "a shorter sibling stays a side branch");
}

/// Each construction no real chain holds panics, naming the rule it breaks
#[test]
fn each_impossible_construction_panics_naming_its_rule() {
    let alice = p2pkh([0xaa; 20]);
    let funded = || {
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 50_000)));
        chain
    };
    let spend = |t: TxBuilder| t.spend(outpoint([0x10; 32], 0));
    type Case<'a> = (&'a str, Box<dyn Fn() + 'a>);
    #[rustfmt::skip]
    let cases: Vec<Case> = vec![
        ("not unspent on this branch", Box::new(|| {
            funded().mine(|b| b.tx(|t| t.spend(outpoint([0x99; 32], 0))));
        })),
        ("not unspent on this branch", Box::new(|| {
            let mut chain = funded();
            chain.mine(|b| b.tx(spend));
            chain.mine(|b| b.tx(spend));
        })),
        ("not unspent on this branch", Box::new(|| {
            funded().mine(|b| b.tx(spend).tx(spend));
        })),
        ("not unspent on this branch", Box::new(|| {
            let mut chain = funded();
            let genesis = chain.genesis();
            chain.branch(genesis).mine(|b| b.tx(spend));
        })),
        ("overspends by 1", Box::new(|| {
            funded().mine(|b| b.tx(|t| spend(t).pay(&alice, 50_001)));
        })),
        ("stated fee", Box::new(|| {
            funded().mine(|b| b.tx(|t| spend(t).pay(&alice, 49_000).fee(500)));
        })),
        ("txid repeated on this branch", Box::new(|| {
            funded().mine(|b| b.tx(|t| t.txid([0x10; 32])));
        })),
        ("pool data before Sapling", Box::new(|| {
            let mut chain = MockChain::regtest().upgrades(Upgrades::all_at(h(5)));
            chain.mine(|b| b.tx(|t| t.sapling_output(1)));
        })),
        ("pool data before Nu5", Box::new(|| {
            let upgrades = Upgrades::all_at(h(1)).onward(NetworkUpgrade::Nu5, h(3));
            MockChain::regtest().upgrades(upgrades).mine(|b| b.coinbase(|c| c.orchard_action([1; 32], 1)));
        })),
        ("pool data before Nu6.3", Box::new(|| {
            let upgrades = Upgrades::all_at(h(1)).without(NetworkUpgrade::Nu6_3);
            MockChain::regtest().upgrades(upgrades).mine(|b| b.tx(|t| t.ironwood_action([1; 32], 1)));
        })),
        ("nullifier revealed twice", Box::new(|| {
            let mut chain = MockChain::regtest();
            chain.mine(|b| b.tx(|t| t.sapling_spend([5; 32])));
            chain.mine(|b| b.tx(|t| t.sapling_spend([5; 32])));
        })),
        ("orchard nullifier revealed twice", Box::new(|| {
            let mut chain = MockChain::regtest();
            chain.mine(|b| b.coinbase(|c| c.orchard_action([6; 32], 1)));
            chain.mine(|b| b.coinbase(|c| c.orchard_action([6; 32], 2)));
        })),
        ("ZIP 209", Box::new(|| {
            funded().mine(|b| b.tx(|t| t.sapling_spend([5; 32]).value_balance(ShieldedPool::Sapling, 1)));
        })),
        ("a coinbase spends nothing", Box::new(|| {
            funded().mine(|b| b.coinbase(spend));
        })),
        ("a coinbase pays no fee", Box::new(|| {
            funded().mine(|b| b.coinbase(|c| c.fee(1)));
        })),
        ("past the supply", Box::new(|| {
            funded().mine(|b| b.coinbase(|c| c.pay(&alice, 21_000_000 * 100_000_000 + 1)));
        })),
        ("not after the median time past", Box::new(|| {
            funded().mine(|b| b.time(GENESIS_TIME));
        })),
        ("bits() under Work::Limit", Box::new(|| {
            funded().mine(|b| b.bits(REGTEST_BITS));
        })),
        ("outweigh() under Work::Limit", Box::new(|| {
            funded().fork(h(0)).outweigh();
        })),
        ("bits() on an outweigh() block", Box::new(|| {
            MockChain::regtest().varied_work().fork(h(0)).outweigh().mine(|b| b.bits(REGTEST_BITS));
        })),
        ("configured after its first mine", Box::new(|| {
            funded().varied_work();
        })),
        ("Sapling at 5 activates before Overwinter", Box::new(|| {
            let upgrades = Upgrades::all_at(h(6)).with(NetworkUpgrade::Sapling, h(5));
            MockChain::regtest().upgrades(upgrades);
        })),
        ("above the best tip", Box::new(|| {
            funded().at(h(2));
        })),
    ];
    for (rule, case) in cases {
        let panic = catch_unwind(AssertUnwindSafe(case)).expect_err(rule);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|message| message.to_string()))
            .unwrap_or_default();
        assert!(message.contains(rule), "expected `{rule}`, got `{message}`");
    }
}

/// M5: `outweigh()` = the best right after its first block, by the least work that wins
/// - retreat (12 → 11), same-height replacement, revival of an old tip; later blocks at the limit
/// - one nBits step easier would have lost; a side branch never moves the tip
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
