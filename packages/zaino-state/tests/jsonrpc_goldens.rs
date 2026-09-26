//! Pins the JSON of every JSON-RPC type Zaino serves from `zaino-state` to golden files.

#![forbid(unsafe_code)]

#[path = "support/golden.rs"]
mod golden;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use zaino_chain_store_zainodb::tests::vectors::{load_vector_blocks, VectorBlock};
use zaino_common::network::ActivationHeights;
use zaino_primitives::types::{SignedZatoshis, ValuePoolBalance, Zatoshis};
use zebra_chain::block::{self, SerializedBlock};
use zebra_chain::parameters::Network;
use zebra_chain::serialization::ZcashSerialize as _;
use zebra_chain::transaction::{SerializedTransaction, Transaction};

use zaino_state::jsonrpc_types::{
    BlockObject, BlockchainValuePoolBalances, GetAddressBalanceRequest, GetAddressTxIdsRequest,
    GetBlock, GetBlockHash, GetBlockTransaction, GetBlockTrees, GetBlockchainInfoBalance,
    GetRawTransaction, LegacyCode, TransactionObject,
};

/// The directory holding this suite's golden files.
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("jsonrpc")
}

/// Asserts that `value` matches the golden file `name` in this suite's directory.
fn assert_golden<T: serde::Serialize>(name: &str, value: &T) {
    golden::assert_golden(&golden_dir(), name, value);
}

/// The network the vector chain was mined on.
fn network() -> Network {
    ActivationHeights::default().to_regtest_network()
}

/// A fixed block time, so a golden never depends on the clock.
fn block_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("the fixed timestamp is in range")
}

/// A domain pool balance from constants the test chose.
fn pool(id: &str, value: u64, delta: Option<i64>) -> ValuePoolBalance {
    ValuePoolBalance {
        id: id.to_string(),
        chain_value: Zatoshis::new(value).expect("the test amount is in range"),
        monitored: true,
        value_delta: delta
            .map(|delta| SignedZatoshis::try_new(delta).expect("the test delta is in range")),
    }
}

/// The vector chain, which spans coinbase-only, transparent, Sapling and Orchard transactions.
fn vector_blocks() -> Vec<VectorBlock> {
    load_vector_blocks().expect("the vector chain loads")
}

/// One rendered case: a named vector block and the transaction in it that gives the case its name.
struct Case<'a> {
    name: &'static str,
    block: &'a VectorBlock,
    transaction: Arc<Transaction>,
}

/// The first vector block holding a transaction that satisfies `predicate`, as a case named `name`.
fn first_case_where<'a>(
    blocks: &'a [VectorBlock],
    name: &'static str,
    predicate: impl Fn(&Transaction) -> bool,
) -> Case<'a> {
    blocks
        .iter()
        .find_map(|block| {
            block
                .zebra_block
                .transactions
                .iter()
                .find(|transaction| predicate(transaction))
                .map(|transaction| Case {
                    name,
                    block,
                    transaction: transaction.clone(),
                })
        })
        .expect("the vector chain holds a transaction of every kind this suite covers")
}

/// The cases this suite renders: genesis, the first block, and the first transaction of each kind.
fn selected_cases(blocks: &[VectorBlock]) -> Vec<Case<'_>> {
    vec![
        first_case_where(&blocks[..1], "genesis", Transaction::is_coinbase),
        first_case_where(&blocks[1..2], "h1", Transaction::is_coinbase),
        first_case_where(blocks, "transparent_spend", |tx| {
            tx.inputs().iter().any(|input| input.outpoint().is_some())
        }),
        first_case_where(blocks, "sapling_output", |tx| {
            tx.sapling_outputs().count() > 0
        }),
        first_case_where(blocks, "sapling_spend", |tx| {
            tx.sapling_spends_per_anchor().count() > 0
        }),
        first_case_where(blocks, "orchard", |tx| tx.orchard_shielded_data().is_some()),
    ]
}

/// Renders `block` at `verbosity` exactly as the validator source does, with fixed chain-state facts.
fn block_response(vector: &VectorBlock, verbosity: u8, next: Option<block::Hash>) -> GetBlock {
    let block = &vector.zebra_block;
    let raw = block
        .zcash_serialize_to_vec()
        .expect("a vector block serializes");
    if verbosity == 0 {
        return GetBlock::Raw(SerializedBlock::from(raw));
    }

    let block_hash = block.hash();
    let height = block
        .coinbase_height()
        .expect("every vector block has a coinbase height");
    let confirmations = 7;

    let tx = if verbosity >= 2 {
        block
            .transactions
            .iter()
            .map(|transaction| {
                GetBlockTransaction::Object(Box::new(TransactionObject::from_transaction(
                    transaction.clone(),
                    Some(height),
                    Some(confirmations),
                    &network(),
                    Some(block_time()),
                    Some(block_hash),
                    Some(true),
                    transaction.hash(),
                )))
            })
            .collect()
    } else {
        block
            .transactions
            .iter()
            .map(|transaction| GetBlockTransaction::Hash(transaction.hash()))
            .collect()
    };

    GetBlock::Object(Box::new(BlockObject {
        hash: block_hash,
        confirmations,
        size: Some(raw.len() as i64),
        height: Some(height),
        version: Some(block.header.version),
        merkle_root: Some(block.header.merkle_root),
        block_commitments: Some(*block.header.commitment_bytes),
        final_sapling_root: Some(<[u8; 32]>::from(vector.sapling_root)),
        final_orchard_root: Some(<[u8; 32]>::from(vector.orchard_root)),
        n_tx: block.transactions.len(),
        tx,
        time: Some(block_time().timestamp()),
        nonce: Some(*block.header.nonce),
        solution: Some(block.header.solution),
        bits: Some(block.header.difficulty_threshold),
        difficulty: Some(1.5),
        chain_supply: Some(GetBlockchainInfoBalance::empty_chain_supply()),
        value_pools: Some(value_pools()),
        trees: GetBlockTrees::new(vector.sapling_tree_size, vector.orchard_tree_size, 0),
        previous_block_hash: Some(block.header.previous_block_hash),
        next_block_hash: next,
    }))
}

/// A value-pool array with a distinct amount and delta in every slot, the lockbox named by zcashd's alias.
fn value_pools() -> BlockchainValuePoolBalances {
    GetBlockchainInfoBalance::value_pools_from_domain(&[
        pool("transparent", 1_000, Some(-1)),
        pool("sprout", 2_000, None),
        pool("sapling", 3_000, Some(3)),
        pool("orchard", 4_000, Some(-4)),
        pool("deferred", 5_000, Some(5)),
        pool("ironwood", 6_000, None),
    ])
    .expect("every test pool has a slot")
}

#[test]
fn getblock_renders_every_verbosity() {
    let blocks = vector_blocks();
    for case in selected_cases(&blocks) {
        let next = blocks
            .get(case.block.height as usize + 1)
            .map(|next| next.zebra_block.hash());
        for verbosity in [0, 1, 2] {
            assert_golden(
                &format!("getblock_{}_verbosity{verbosity}", case.name),
                &block_response(case.block, verbosity, next),
            );
        }
    }
}

#[test]
fn getrawtransaction_renders_every_chain_position() {
    let blocks = vector_blocks();
    for Case {
        name,
        block: vector,
        transaction,
    } in selected_cases(&blocks)
    {
        let block = &vector.zebra_block;
        let height = block.coinbase_height();

        assert_golden(
            &format!("getrawtransaction_{name}_raw"),
            &GetRawTransaction::Raw(SerializedTransaction::from(transaction.clone())),
        );
        for (position, block_hash, in_active_chain) in [
            ("mempool", None, None),
            ("side_chain", Some(block.hash()), Some(false)),
            ("best_chain", Some(block.hash()), Some(true)),
        ] {
            assert_golden(
                &format!("getrawtransaction_{name}_{position}"),
                &GetRawTransaction::Object(Box::new(TransactionObject::from_transaction(
                    transaction.clone(),
                    height,
                    Some(3),
                    &network(),
                    block_hash.map(|_| block_time()),
                    block_hash,
                    in_active_chain,
                    transaction.hash(),
                ))),
            );
        }
    }
}

#[test]
fn getblockhash_renders_display_order_hex() {
    let blocks = vector_blocks();
    assert_golden(
        "getblockhash",
        &GetBlockHash::new(blocks[1].zebra_block.hash()),
    );
}

#[test]
fn value_pool_balances_render_each_pool() {
    let mut balances = value_pools().to_vec();
    balances.extend(
        GetBlockchainInfoBalance::value_pools_from_domain(&[])
            .expect("no pools means no unknown pool"),
    );
    balances.push(GetBlockchainInfoBalance::empty_chain_supply());
    assert_golden("value_pool_balances", &balances);
}

#[test]
fn address_requests_accept_every_client_form() {
    let address = "tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd";
    let balance_forms = BTreeMap::from([
        ("single", format!("\"{address}\"")),
        ("object", format!("{{\"addresses\":[\"{address}\"]}}")),
    ]);
    for (form, input) in balance_forms {
        let request: GetAddressBalanceRequest =
            serde_json::from_str(&input).expect("the balance request form parses");
        assert_golden(&format!("getaddressbalance_request_{form}"), &request);
    }

    let txid_forms = BTreeMap::from([
        ("single", format!("\"{address}\"")),
        ("object", format!("{{\"addresses\":[\"{address}\"]}}")),
        (
            "ranged",
            format!("{{\"addresses\":[\"{address}\"],\"start\":1,\"end\":9}}"),
        ),
    ]);
    for (form, input) in txid_forms {
        let request: GetAddressTxIdsRequest =
            serde_json::from_str(&input).expect("the txid request form parses");
        assert_golden(&format!("getaddresstxids_request_{form}"), &request);
    }
}

#[test]
fn legacy_codes_keep_zcashd_numbering() {
    let captured: BTreeMap<String, i32> = serde_json::from_str(
        &std::fs::read_to_string(golden_dir().join("legacy_codes.json"))
            .expect("zebra's captured numbering is present"),
    )
    .expect("zebra's captured numbering parses");

    for (name, code) in [
        ("Misc", LegacyCode::Misc),
        ("InvalidAddressOrKey", LegacyCode::InvalidAddressOrKey),
        ("InvalidParameter", LegacyCode::InvalidParameter),
    ] {
        assert_eq!(
            Some(&i32::from(code)),
            captured.get(name),
            "{name} is numbered as zebra numbered it"
        );
    }
}
