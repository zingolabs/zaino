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

fn paid(zats: u64) -> Fee {
    Fee::Paid(Zatoshis::from_u64(zats).expect("in supply"))
}

/// M1: genesis pinned field by field, consensus order
#[test]
fn genesis_header_bytes_are_pinned_field_by_field() {
    let chain = MockChain::regtest();

    let bytes = chain.header_bytes(chain.genesis().hash);

    let zeros = "00".repeat(32);
    #[rustfmt::skip]
    let golden = [
        ("version",           "04000000".to_owned()),
        ("prev_hash",         zeros.clone()),
        ("merkle_root",       "8fd4dec6203afc91016e177642dc163ecb0dfe49f4182040be3c717428ddaee4".into()),
        ("block_commitments", zeros.clone()),
        ("time",              "00f15365".into()),
        ("bits",              "0f0f0f20".into()),
        ("nonce",             zeros),
        ("solution_len",      "24".into()),
        ("solution",          "00".repeat(36)),
    ];
    let mut at = 0;
    for (field, expected) in golden {
        let len = expected.len() / 2;
        let actual = bytes.get(at..at + len).map(hex);
        assert_eq!(actual.as_deref(), Some(expected.as_str()), "genesis {field} at byte {at}");
        at += len;
    }
    assert_eq!(bytes.len(), at, "genesis: no bytes after the solution");
}

/// M1–M4 on a three-block chain (genesis, bare block, Alice's coinbase + a Sapling tx)
#[test]
fn every_held_block_is_the_decode_of_its_own_bytes_and_links_to_its_parent() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    chain.mine_empty(1);
    let two = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)).tx(|t| t.sapling_output(1)));

    let blocks = chain.blocks(two);

    let heights: Vec<BlockHeight> = blocks.iter().map(|block| block.header().height).collect();
    assert_eq!(heights, [h(0), h(1), h(2)], "blocks(tip) = genesis ..= tip");
    for block in &blocks {
        let header = block.header();
        let height = header.height;
        let decoded = crate::decode_block(chain.block_bytes(header.hash)).expect("decodes");
        assert_eq!(format!("{decoded:?}"), format!("{block:?}"), "{height}: held = decoded bytes");
        assert_eq!(sha256d(&chain.header_bytes(header.hash)), header.hash.0, "{height}: hash");
        let txids: Vec<TxId> = block.transactions().iter().map(|tx| tx.txid).collect();
        let root = MerkleRoot::of_txids(&txids);
        assert_eq!(root, Some(header.merkle_root), "{height}: merkle root of {txids:?}");
        let coinbases: Vec<bool> =
            block.transactions().iter().map(|tx| tx.transparent.coinbase).collect();
        let slot_zero_only: Vec<bool> = (0..coinbases.len()).map(|slot| slot == 0).collect();
        assert_eq!(coinbases, slot_zero_only, "{height}: one coinbase, slot 0");
        for txid in txids {
            let bytes = chain.tx_bytes(txid).expect("every minted tx keeps its bytes");
            let reread = crate::decode_transaction(bytes).expect("decodes").txid;
            assert_eq!(reread, txid, "{height}: txid = hash of its kept bytes");
        }
    }
    for pair in blocks.windows(2) {
        let (parent, child) = (pair[0].header(), pair[1].header());
        let height = child.height;
        assert_eq!(child.prev_hash, parent.hash, "{height}: prev_hash = parent's hash");
        assert_eq!(height, parent.height + 1, "{height}: parent's height + 1");
    }
    assert_eq!(chain.tx(two, 0).transparent.outputs[0].script, alice, "coinbase as stated");
}

/// Siblings on one parent at one time still hash apart (header nonce = mint counter)
#[test]
fn siblings_are_distinct_and_a_tie_keeps_the_first_mined() {
    let mut chain = MockChain::regtest();
    let genesis = chain.genesis();

    let first = chain.mine_empty(1);
    let sibling = chain.branch(genesis).mine_empty(1).tip();

    assert_ne!(first.hash, sibling.hash, "same parent, same time: distinct blocks");
    assert_eq!(chain.tip(), first, "a tie keeps the first mined");
}

#[test]
fn genesis_with_mines_genesis_as_stated() {
    let alice = p2pkh([0xaa; 20]);

    let funded = MockChain::regtest().genesis_with(|b| b.coinbase(|c| c.pay(&alice, 7_000)));

    let paid =
        TransparentOutput { value: Zatoshis::from_u64(7_000).expect("in supply"), script: alice };
    let outputs = &funded.tx(funded.genesis(), 0).transparent.outputs;
    assert_eq!(outputs, &[paid], "genesis coinbase pays as stated");
    assert_ne!(funded.genesis(), MockChain::regtest().genesis(), "a bare genesis differs");
}

/// M2 default time = parent + target spacing (150 s pre-Blossom, 75 s, 25 s from NU7)
#[test]
fn each_child_is_one_target_spacing_after_its_parent() {
    let early = Some(h(1));
    let upgrades =
        LocalNetwork { overwinter: early, sapling: early, nu7: Some(h(5)), ..all_at(h(3)) };
    let mut chain = MockChain::regtest().upgrades(upgrades);

    let six = chain.mine_empty(6);

    let times: Vec<u32> = chain.blocks(six).iter().map(|block| block.header().time).collect();
    let spacing: Vec<u32> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(spacing, [150, 150, 75, 75, 25, 25], "heights 1..=6: Blossom at 3, NU7 at 5");
}

#[derive(Debug, PartialEq)]
struct Format {
    version: TxVersion,
    embedded_branch: Option<BranchId>,
}

/// v1 before Overwinter, v3, v4 from Sapling, v5 from NU5 (branch embedded), Sprout = v4,
/// Ironwood = v6
#[test]
fn each_transaction_is_written_in_its_heights_format() {
    let upgrades = LocalNetwork::from_activation_heights(|upgrade| match upgrade {
        NetworkUpgrade::Overwinter => Some(h(1)),
        NetworkUpgrade::Sapling => Some(h(2)),
        NetworkUpgrade::Nu7 => None,
        NetworkUpgrade::Nu6_3 => Some(h(4)),
        _ => Some(h(3)),
    });
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest().upgrades(upgrades);
    chain.mine_empty(3);
    let (ironwood_slot, sprout_slot) = (1, 2);
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
        Format { version: tx.version(), embedded_branch: embeds.then(|| tx.consensus_branch_id()) }
    };

    #[rustfmt::skip]
    let coinbases = [
        (0, TxVersion::Sprout(1), None),
        (1, TxVersion::V3,        None),
        (2, TxVersion::V4,        None),
        (3, TxVersion::V5,        Some(BranchId::Nu6_2)),
        (4, TxVersion::V5,        Some(BranchId::Nu6_3)),
    ];
    for (height, version, embedded_branch) in coinbases {
        let expected = Format { version, embedded_branch };
        assert_eq!(format(chain.at(h(height)), 0), expected, "coinbase at {height}");
    }
    let ironwood = Format { version: TxVersion::V6, embedded_branch: Some(BranchId::Nu6_3) };
    assert_eq!(format(four, ironwood_slot), ironwood, "Ironwood action at 4");
    let sprout = Format { version: TxVersion::V4, embedded_branch: None };
    assert_eq!(format(four, sprout_slot), sprout, "Sprout JoinSplit at 4");
}

/// M6: one block spending Alice's 50 000 coinbase, then the outputs of its own earlier slots
///
/// - slot 1: 30 000 to Bob + 19 000 change, fee stated 1 000
/// - slot 2: the change → 18 500 to Bob (fee derived 500)
/// - slot 3: Bob's 30 000 → 29 000 into Sapling (fee derived 1 000)
#[test]
fn spends_in_one_block_chain_in_slot_order_and_each_fee_is_what_conservation_leaves() {
    let (alice, bob) = (p2pkh([0xaa; 20]), p2pkh([0xbb; 20]));
    let (to_bob, change) = (0, 1);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);

    let spent = chain.mine(|b| {
        b.tx(|t| t.spend(funding.clone()).pay(&bob, 30_000).pay(&alice, 19_000).fee(1_000))
            .tx(|t| t.spend_in_block(1, change).pay(&bob, 18_500))
            .tx(|t| {
                t.spend_in_block(1, to_bob)
                    .sapling_output(7)
                    .value_balance(ShieldedPool::Sapling, -29_000)
            })
    });

    let fees = chain.fees(spent.hash);
    let expected = vec![Fee::Coinbase, paid(1_000), paid(500), paid(1_000)];
    assert_eq!(fees, BlockFees { height: h(2), hash: spent.hash, fees: expected }, "fees by slot");
    let slot_one = chain.tx(spent, 1).txid;
    let inputs = |slot: usize| chain.tx(spent, slot).transparent.inputs.clone();
    assert_eq!(inputs(1), [funding], "slot 1 spends the coinbase");
    assert_eq!(inputs(2), [OutPoint::new(slot_one.into(), change)], "slot 2 spends the change");
    assert_eq!(inputs(3), [OutPoint::new(slot_one.into(), to_bob)], "slot 3 spends Bob's 30 000");
}

/// M6 ZIP 209: 29 000 into Sapling, then 20 000 back out; 18 000 into Sprout, then 9 000 out
#[test]
fn value_leaving_a_shielded_pool_pays_out_and_each_fee_is_what_conservation_leaves() {
    let (alice, bob) = (p2pkh([0xaa; 20]), p2pkh([0xbb; 20]));
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000).pay(&bob, 18_500)));
    let (alice_funds, bob_funds) = (chain.output(funded, 0, 0), chain.output(funded, 0, 1));
    chain.mine(|b| {
        b.tx(|t| {
            let shielded = t.spend(alice_funds).sapling_output(7);
            shielded.value_balance(ShieldedPool::Sapling, -29_000).pay(&alice, 20_000)
        })
    });

    let withdrawn = chain.mine(|b| {
        b.tx(|t| {
            t.sapling_spend(5)
                .value_balance(ShieldedPool::Sapling, 20_000)
                .pay(&alice, 19_900)
                .fee(100)
        })
        .tx(|t| t.spend(bob_funds).sprout_balance(-18_000).fee(500))
        .tx(|t| t.sprout_balance(9_000).pay(&bob, 8_000))
    });

    let fees = chain.fees(withdrawn.hash).fees;
    assert_eq!(fees, [Fee::Coinbase, paid(100), paid(500), paid(1_000)], "withdrawal fees by slot");
}

#[test]
fn note_tags_land_as_their_canonical_nullifiers_and_commitments() {
    let mut chain = MockChain::regtest();

    let tagged = chain
        .mine(|b| b.tx(|t| t.sapling_spend(5).sapling_output(7)).tx(|t| t.orchard_action(4, 9)));

    let (sapling, orchard) = (&chain.tx(tagged, 1).sapling, &chain.tx(tagged, 2).orchard);
    assert_eq!(sapling.spends[0].nullifier, nullifier(5), "sapling_spend(5)");
    assert_eq!(sapling.outputs[0].cmu, commitment(7), "sapling_output(7)");
    assert_eq!(orchard.actions[0].nullifier, nullifier(4), "orchard_action(4, _)");
    assert_eq!(orchard.actions[0].cmx, commitment(9), "orchard_action(_, 9)");
    let keys = [sapling.outputs[0].ephemeral_key, orchard.actions[0].ephemeral_key];
    assert_ne!(keys[0], keys[1], "detection material distinct per pool");
}

/// Txids distinct per branch, not chain-wide (a reorged tx re-mined from its bytes)
#[test]
fn a_sibling_branch_may_remine_a_transaction_from_its_bytes() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let spent = chain.mine(|b| b.tx(|t| t.spend(funding).pay(&alice, 49_000)));
    let longer = chain.mine_empty(1);
    let resent = chain.tx_bytes(chain.tx(spent, 1).txid).expect("minted").to_vec();

    let remined = chain.branch(funded).mine(|b| b.raw_tx(resent)).tip();

    assert_eq!(chain.tx(remined, 1).txid, chain.tx(spent, 1).txid, "same bytes, same txid");
    assert_eq!(chain.fees(remined.hash).fees, [Fee::Coinbase, paid(1_000)], "same fee");
    assert_eq!(chain.tip(), longer, "a shorter sibling stays a side branch");
}

/// One refused construction: `before` mined first, then `attempt` must be refused as `refused`
struct Refusal {
    case: &'static str,
    upgrades: LocalNetwork,
    before: fn(&mut MockChain, &OutPoint),
    attempt: fn(&mut MockChain, &OutPoint) -> Result<BlockRef, Refused>,
    refused: fn(&OutPoint) -> Refused,
}

/// M2, M3, M4, M6, M7: every case on Alice's 50 000 coinbase at 1 (`funding`)
#[test]
fn the_ledger_refuses_what_no_real_chain_holds() {
    let all = all_at(h(1));
    let nothing_before: fn(&mut MockChain, &OutPoint) = |_, _| {};
    let orchard_late = LocalNetwork::from_activation_heights(|upgrade| match upgrade {
        NetworkUpgrade::Nu7 => None,
        NetworkUpgrade::Nu5 | NetworkUpgrade::Nu6 | NetworkUpgrade::Nu6_1 => Some(h(3)),
        NetworkUpgrade::Nu6_2 | NetworkUpgrade::Nu6_3 => Some(h(3)),
        _ => Some(h(1)),
    });
    let cases = [
        Refusal {
            case: "spend of an output never mined",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.tx(|t| t.spend(OutPoint::new([0x99; 32], 0)))),
            refused: |_| Refused::Unspent(OutPoint::new([0x99; 32], 0)),
        },
        Refusal {
            case: "spend of an output spent a block before",
            upgrades: all,
            before: |chain, funding| {
                chain.mine(|b| b.tx(|t| t.spend(funding.clone())));
            },
            attempt: |chain, funding| chain.try_mine(|b| b.tx(|t| t.spend(funding.clone()))),
            refused: |funding| Refused::Unspent(funding.clone()),
        },
        Refusal {
            case: "two spends of one output in one block",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, funding| {
                chain.try_mine(|b| {
                    b.tx(|t| t.spend(funding.clone())).tx(|t| t.spend(funding.clone()))
                })
            },
            refused: |funding| Refused::Unspent(funding.clone()),
        },
        Refusal {
            case: "spend of an output off its branch",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, funding| {
                let genesis = chain.genesis();
                let side = chain.branch(genesis).try_mine(|b| b.tx(|t| t.spend(funding.clone())));
                side.map(|branch| branch.tip())
            },
            refused: |funding| Refused::Unspent(funding.clone()),
        },
        Refusal {
            case: "paying 1 past the inputs",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, funding| {
                chain.try_mine(|b| {
                    b.tx(|t| t.spend(funding.clone()).pay(&p2pkh([0xaa; 20]), 50_001))
                })
            },
            refused: |_| Refused::Overspends(1),
        },
        Refusal {
            case: "stated fee 500, conservation leaves 1 000",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, funding| {
                let alice = p2pkh([0xaa; 20]);
                chain.try_mine(|b| b.tx(|t| t.spend(funding.clone()).pay(&alice, 49_000).fee(500)))
            },
            refused: |_| Refused::StatedFee { stated: 500, derived: 1_000 },
        },
        Refusal {
            case: "transaction mined twice on its branch",
            upgrades: all,
            before: |chain, _| {
                chain.mine(|b| b.tx(|t| t.sapling_output(1)));
            },
            attempt: |chain, _| {
                let again = chain.tx_bytes(chain.tx(chain.tip(), 1).txid).expect("minted");
                let again = again.to_vec();
                chain.try_mine(|b| b.raw_tx(again))
            },
            refused: |_| Refused::Repeated,
        },
        Refusal {
            case: "Sapling output at 2, Sapling at 5",
            upgrades: all_at(h(5)),
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.tx(|t| t.sapling_output(1))),
            refused: |_| Refused::Inactive(NetworkUpgrade::Sapling),
        },
        Refusal {
            case: "Orchard action at 2, NU5 at 3",
            upgrades: orchard_late,
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.coinbase(|c| c.orchard_action(1, 1))),
            refused: |_| Refused::Inactive(NetworkUpgrade::Nu5),
        },
        Refusal {
            case: "Ironwood action, NU6.3 never",
            upgrades: LocalNetwork { nu6_3: None, ..all },
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.tx(|t| t.ironwood_action(1, 1))),
            refused: |_| Refused::Inactive(NetworkUpgrade::Nu6_3),
        },
        Refusal {
            case: "Sapling nullifier 5 revealed again",
            upgrades: all,
            before: |chain, _| {
                chain.mine(|b| b.tx(|t| t.sapling_spend(5)));
            },
            attempt: |chain, _| chain.try_mine(|b| b.tx(|t| t.sapling_spend(5))),
            refused: |_| Refused::Revealed(ShieldedPool::Sapling),
        },
        Refusal {
            case: "Orchard nullifier 6 revealed again",
            upgrades: all,
            before: |chain, _| {
                chain.mine(|b| b.coinbase(|c| c.orchard_action(6, 1)));
            },
            attempt: |chain, _| chain.try_mine(|b| b.coinbase(|c| c.orchard_action(6, 2))),
            refused: |_| Refused::Revealed(ShieldedPool::Orchard),
        },
        Refusal {
            case: "1 out of an empty Sapling pool",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, _| {
                chain.try_mine(|b| {
                    b.tx(|t| t.sapling_spend(5).value_balance(ShieldedPool::Sapling, 1))
                })
            },
            refused: |_| Refused::Drained(1),
        },
        Refusal {
            case: "coinbase spending an output",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, funding| chain.try_mine(|b| b.coinbase(|c| c.spend(funding.clone()))),
            refused: |_| Refused::CoinbaseSpends,
        },
        Refusal {
            case: "coinbase stating a fee",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.coinbase(|c| c.fee(1))),
            refused: |_| Refused::CoinbaseFee,
        },
        Refusal {
            case: "coinbase paying MAX_MONEY + 1",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, _| {
                chain.try_mine(|b| b.coinbase(|c| c.pay(&p2pkh([0xaa; 20]), MAX_MONEY + 1)))
            },
            refused: |_| Refused::PastSupply,
        },
        Refusal {
            case: "time = median time past (upper median of genesis, genesis + 75)",
            upgrades: all,
            before: nothing_before,
            attempt: |chain, _| chain.try_mine(|b| b.time(GENESIS_TIME + 75)),
            refused: |_| {
                let median = GENESIS_TIME + 75;
                Refused::NotAfterMedianTime { time: median, median }
            },
        },
    ];
    for Refusal { case, upgrades, before, attempt, refused } in cases {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().upgrades(upgrades);
        let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
        let funding = chain.output(funded, 0, 0);
        before(&mut chain, &funding);
        let (tip, held) = (chain.tip(), chain.blocks.len());

        let outcome = attempt(&mut chain, &funding);

        assert_eq!(outcome, Err(refused(&funding)), "{case}");
        assert_eq!((chain.tip(), chain.blocks.len()), (tip, held), "{case}: nothing mined");
    }
}

/// M2 boundary: the least time accepted = median time past + 1
#[test]
fn a_block_one_second_after_the_median_time_past_is_mined() {
    let mut chain = MockChain::regtest();

    let mined = chain.try_mine(|b| b.time(GENESIS_TIME + 1));

    let held = mined.map(|at| chain.block(at.hash).header().time);
    assert_eq!(held, Ok(GENESIS_TIME + 1), "genesis median = {GENESIS_TIME}");
}

/// `mine` = `try_mine` + a panic naming the height and the rule
#[test]
#[should_panic(expected = "mine at 1: takes 1 from a pool holding less (ZIP 209)")]
fn mining_a_refused_block_panics_naming_its_height_and_rule() {
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.tx(|t| t.sapling_spend(5).value_balance(ShieldedPool::Sapling, 1)));
}

/// Total work along genesis ..= `tip`
fn work(chain: &MockChain, tip: BlockRef) -> u128 {
    chain.blocks(tip).iter().map(|block| block.header().bits.work()).sum()
}

/// M5 minimal margin: 12 → 11 by `outweigh`, one nBits step easier would have lost
#[test]
fn outweigh_retreats_the_tip_by_the_least_work_that_wins() {
    const MAX_MANTISSA: u32 = 0x7f_ffff;
    let mut chain = MockChain::regtest().varied_work();
    let twelve = chain.mine_empty(12);

    let retreat = chain.fork(h(10)).outweigh().mine_empty(1).tip();

    assert_eq!(chain.tip(), retreat, "retreat at {} = the best", retreat.height);
    assert_eq!(retreat.height, h(11), "fork at 10 + 1 block");
    let (won, lost) = (work(&chain, retreat), work(&chain, twelve));
    assert!(won > lost, "retreat work {won} > old tip work {lost}");
    let bits = chain.block(retreat.hash).header().bits;
    let mantissa = bits.bits() & 0x00ff_ffff;
    assert!(mantissa < MAX_MANTISSA, "next-easier nBits exists: mantissa {mantissa:#x}");
    let easier = CompactDifficulty::try_from_bits(bits.bits() + 1).expect("valid nBits");
    let with_easier = won - bits.work() + easier.work();
    assert!(with_easier <= lost, "one step easier: {with_easier} <= old tip {lost} (would lose)");
}

#[test]
fn outweigh_replaces_the_tip_at_the_same_height() {
    let mut chain = MockChain::regtest().varied_work();
    let eleven = chain.mine_empty(11);

    let replaced = chain.fork(h(10)).outweigh().mine_empty(1).tip();

    assert_eq!(chain.tip(), replaced, "replacement = the best");
    assert_eq!(replaced.height, eleven.height, "same height");
    assert_ne!(replaced, eleven, "other block");
}

/// Old tip at 12 revived by 3 blocks: 13 outweighs the best, 14 and 15 at the limit
#[test]
fn outweigh_revives_an_old_tip_with_only_its_first_block_heavier() {
    let mut chain = MockChain::regtest().varied_work();
    let twelve = chain.mine_empty(12);
    chain.fork(h(10)).outweigh().mine_empty(1);

    let revived = chain.branch(twelve).outweigh().mine_empty(3).tip();

    assert_eq!(chain.tip(), revived, "revived branch = the best");
    assert_eq!(chain.at(h(12)), twelve, "old tip back on the best");
    for height in [h(14), h(15)] {
        let bits = chain.block(chain.at(height).hash).header().bits.bits();
        assert_eq!(bits, REGTEST_BITS, "{height}: after the outweighing block, the limit");
    }
}

#[test]
fn a_side_branch_never_moves_the_tip() {
    let mut chain = MockChain::regtest().varied_work();
    let tip = chain.mine_empty(6);

    let side = chain.fork(h(5)).mine_empty(1).tip();

    assert_eq!((chain.tip(), side.height), (tip, h(6)), "equal height side branch, tip kept");
}

#[test]
fn varied_work_mines_stated_bits_as_stated() {
    let mut chain = MockChain::regtest().varied_work();

    let stated = chain.mine(|b| b.bits(0x2007_ffff));

    assert_eq!(chain.block(stated.hash).header().bits.bits(), 0x2007_ffff);
}

/// `Work::Limit`: every nBits the limit, so length alone decides
#[test]
fn under_limit_work_a_longer_fork_wins_and_an_equal_one_does_not() {
    let mut chain = MockChain::regtest();
    let three = chain.mine_empty(3);

    let equal = chain.fork(h(1)).mine_empty(2).tip();
    let equal_best = chain.tip();
    let longer = chain.branch(equal).mine_empty(1).tip();

    assert_eq!(equal_best, three, "equal length: the first mined stays best");
    assert_eq!(chain.tip(), longer, "one longer: the fork wins");
    let bits: Vec<u32> = chain.blocks(longer).iter().map(|b| b.header().bits.bits()).collect();
    assert_eq!(bits, [REGTEST_BITS; 5], "genesis ..= 4 at the limit");
}

#[derive(Debug, PartialEq)]
struct Held {
    txid: TxId,
    fee: u64,
}

fn held(chain: &MockChain) -> Vec<Held> {
    let held = chain.mempool().iter();
    held.map(|unmined| Held { txid: unmined.tx.txid, fee: unmined.fee.into_u64() }).collect()
}

/// Alice's 50 000 → `parent` (49 000 back, fee 1 000) → `child` (48 500 back, fee 500)
#[test]
fn the_mempool_holds_each_entry_at_the_fee_its_spends_leave() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);

    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    let child = chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));

    let expected = [Held { txid: parent, fee: 1_000 }, Held { txid: child, fee: 500 }];
    assert_eq!(held(&chain), expected, "submission order, derived fees");
}

#[test]
fn evicting_an_entry_drops_the_entries_spending_it() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));

    chain.evict(parent);

    assert_eq!(held(&chain), [], "child orphaned, gone with its parent");
}

#[test]
fn a_mined_entry_leaves_the_mempool() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    let child = chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));
    let bytes = |txid: TxId| chain.tx_bytes(txid).expect("submitted").to_vec();
    let (parent_bytes, child_bytes) = (bytes(parent), bytes(child));

    let mined = chain.mine(|b| b.raw_tx(parent_bytes).raw_tx(child_bytes));

    let fees = chain.fees(mined.hash).fees;
    assert_eq!(fees, [Fee::Coinbase, paid(1_000), paid(500)], "mined at their held fees");
    assert_eq!(held(&chain), [], "mined = out of the mempool");
}

/// Double spend of a held entry's prevout: refused, nothing held beside it
#[test]
fn a_rival_to_a_held_spend_is_refused() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let first = chain.submit(|t| t.spend(funding.clone()).pay(&alice, 49_000));

    let rival = chain.try_submit(|t| t.spend(funding.clone()).pay(&alice, 48_000));

    assert_eq!(rival, Err(Refused::Unspent(funding)), "prevout held by {first}");
    assert_eq!(held(&chain), [Held { txid: first, fee: 1_000 }], "first entry alone");
}

/// Block 2 mines `parent` + `child` (Alice's coinbase at 1 funds `parent`)
#[test]
fn a_reorg_off_a_block_returns_its_transactions_in_block_order() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest().varied_work();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    let child = chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));
    let bytes = |txid: TxId| chain.tx_bytes(txid).expect("submitted").to_vec();
    let (parent_bytes, child_bytes) = (bytes(parent), bytes(child));
    chain.mine(|b| b.raw_tx(parent_bytes).raw_tx(child_bytes));

    chain.fork(h(1)).outweigh().mine_empty(1);

    let expected = [Held { txid: parent, fee: 1_000 }, Held { txid: child, fee: 500 }];
    assert_eq!(held(&chain), expected, "unmined by the reorg: back, block order");
}

#[test]
fn a_reorg_back_onto_a_block_mines_its_transactions_again() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest().varied_work();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding).pay(&alice, 49_000));
    let parent_bytes = chain.tx_bytes(parent).expect("submitted").to_vec();
    let mined = chain.mine(|b| b.raw_tx(parent_bytes));
    chain.fork(h(1)).outweigh().mine_empty(1);

    let revived = chain.branch(mined).outweigh().mine_empty(1).tip();

    assert_eq!(chain.tip(), revived, "revived branch = the best");
    assert_eq!(held(&chain), [], "mined again: out of the mempool");
}

#[test]
fn a_reorg_onto_a_conflicting_spend_drops_the_entry_and_its_children() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest().varied_work();
    let funded = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 50_000)));
    let funding = chain.output(funded, 0, 0);
    let parent = chain.submit(|t| t.spend(funding.clone()).pay(&alice, 49_000));
    chain.submit(|t| t.spend(OutPoint::new(parent.into(), 0)).pay(&alice, 48_500));

    chain.fork(h(1)).outweigh().mine(|b| b.tx(|t| t.spend(funding).pay(&alice, 40_000)));

    assert_eq!(held(&chain), [], "prevout spent by the winner: parent dropped, child orphaned");
}
