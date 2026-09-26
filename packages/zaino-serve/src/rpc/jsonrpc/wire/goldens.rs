//! Pins the JSON of every response `zaino-serve` renders from the domain to golden files.

use std::path::PathBuf;

use super::{address, address_queries, blockchain_info, hashes, node_info, subtrees, treestate};
use crate::golden;
use zaino_common::network::ActivationHeights;
use zaino_primitives::types::rpc::{NodeInfo, SubtreeRoots};
use zaino_primitives::types::{
    AddressBalance, BlockHash, BlockchainInfo, ConsensusBranchIds, Height, NetworkUpgradeInfo,
    NetworkUpgradeStatus, PoolTreestate, Script, ShieldedPool, SignedZatoshis, SubtreeRoot,
    TransactionId, TransparentAddress, TreeRoot, Treestate, Utxo, ValuePoolBalance, Zatoshis,
    ZatoshisFlowSum,
};

/// Asymmetric under reversal, so a missing or doubled byte-reversal changes the golden.
const ASYMMETRIC: [u8; 32] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01,
];

/// A real testnet P2PKH address, since an invented one fails its checksum.
const ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

/// Asserts that `value` matches the golden file `name` in this suite's directory.
fn assert_golden<T: serde::Serialize>(name: &str, value: &T) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("jsonrpc");
    golden::assert_golden(&dir, name, value);
}

/// A height the test itself knows is in range.
fn height(value: u32) -> Height {
    Height::try_from(value).expect("the test height is in range")
}

/// An amount the test itself knows is in range.
fn zatoshis(value: u64) -> Zatoshis {
    Zatoshis::new(value).expect("the test amount is in range")
}

#[test]
fn getinfo_renders_healthy_and_failing_nodes() {
    let healthy = NodeInfo {
        version: 2_000_000,
        build: "v2.0.0".to_string(),
        subversion: "/Zebra:2.0.0/".to_string(),
        protocol_version: 170_120,
        blocks: height(2_500_000),
        connections: 8,
        difficulty: 1.5,
        testnet: false,
        proxy: None,
        pay_tx_fee: zatoshis(1_000),
        relay_fee: zatoshis(100),
        errors: None,
        errors_timestamp: None,
    };
    assert_golden("getinfo_healthy", &node_info::from_domain(healthy.clone()));

    let failing = NodeInfo {
        proxy: Some("127.0.0.1:9050".to_string()),
        errors: Some("disk full".to_string()),
        errors_timestamp: Some(1_700_000_000),
        testnet: true,
        ..healthy
    };
    assert_golden("getinfo_failing", &node_info::from_domain(failing));
}

#[test]
fn getblockchaininfo_renders_pools_upgrades_and_consensus() {
    let pool = |id: &str, value: u64, delta: Option<i64>| ValuePoolBalance {
        id: id.to_string(),
        chain_value: zatoshis(value),
        monitored: true,
        value_delta: delta
            .map(|delta| SignedZatoshis::try_new(delta).expect("the test delta is in range")),
    };
    let info = BlockchainInfo {
        chain: "regtest".to_string(),
        blocks: height(100),
        headers: height(101),
        estimated_height: height(102),
        best_block_hash: BlockHash::from(ASYMMETRIC),
        difficulty: 1.25,
        verification_progress: 0.5,
        chain_work: None,
        pruned: false,
        size_on_disk: 4_096,
        commitments: 7,
        chain_supply: pool("", 21_000, Some(10)),
        value_pools: vec![
            pool("transparent", 1_000, Some(-1)),
            pool("sapling", 3_000, None),
            pool("orchard", 2_000, Some(2)),
            pool("lockbox", 500, None),
        ],
        upgrades: vec![
            NetworkUpgradeInfo {
                branch_id: 0xc2d6_d0b4u32.into(),
                name: "NU5".to_string(),
                activation_height: height(1),
                status: NetworkUpgradeStatus::Active,
            },
            NetworkUpgradeInfo {
                branch_id: 0xc8e7_1055u32.into(),
                name: "NU6".to_string(),
                activation_height: height(1),
                status: NetworkUpgradeStatus::Pending,
            },
        ],
        consensus: ConsensusBranchIds {
            chain_tip: 0xc2d6_d0b4u32.into(),
            next_block: 0xc8e7_1055u32.into(),
        },
    };
    let network = ActivationHeights::default().to_regtest_network();
    assert_golden(
        "getblockchaininfo",
        &blockchain_info::from_domain(info, &network).expect("the sample renders"),
    );
}

#[test]
fn z_gettreestate_renders_with_and_without_ironwood() {
    let trees = Treestate {
        block_hash: BlockHash::from(ASYMMETRIC),
        height: height(1_000),
        time: 1_700_000_000,
        sapling: Some(PoolTreestate {
            final_root: Some(TreeRoot::from(ASYMMETRIC)),
            final_state: vec![0xde, 0xad],
        }),
        orchard: Some(PoolTreestate {
            final_root: None,
            final_state: vec![0xbe, 0xef],
        }),
        ironwood: None,
    };
    assert_golden(
        "z_gettreestate_pre_ironwood",
        &treestate::from_domain(trees.clone()),
    );

    let with_ironwood = Treestate {
        sapling: None,
        ironwood: Some(PoolTreestate {
            final_root: Some(TreeRoot::from(ASYMMETRIC)),
            final_state: vec![0xca, 0xfe],
        }),
        ..trees
    };
    assert_golden(
        "z_gettreestate_ironwood",
        &treestate::from_domain(with_ironwood),
    );
}

#[test]
fn z_getsubtreesbyindex_renders_each_pool() {
    for (name, pool) in [
        ("sapling", ShieldedPool::Sapling),
        ("orchard", ShieldedPool::Orchard),
        ("ironwood", ShieldedPool::Ironwood),
    ] {
        let roots = SubtreeRoots {
            pool,
            start_index: 3,
            subtrees: vec![SubtreeRoot {
                root: TreeRoot::from(ASYMMETRIC),
                end_height: height(2_000),
            }],
        };
        assert_golden(
            &format!("z_getsubtreesbyindex_{name}"),
            &subtrees::from_domain(roots),
        );
    }
}

#[test]
fn address_queries_render_in_zatoshis() {
    let balance = AddressBalance {
        balance: zatoshis(150_000_000),
        received: ZatoshisFlowSum::from_summed(200_000_000),
    };
    assert_golden(
        "getaddressbalance",
        &address_queries::address_balance_from_domain(balance).expect("the sample renders"),
    );

    let utxo = Utxo {
        address: TransparentAddress::try_new(ADDRESS).expect("the address is valid"),
        txid: TransactionId::from(ASYMMETRIC),
        output_index: 2,
        script: Script::from(vec![0x76, 0xa9]),
        satoshis: zatoshis(50_000),
        height: height(1_234),
    };
    assert_golden(
        "getaddressutxos",
        &address_queries::address_utxos_from_domain(vec![utxo]).expect("the sample renders"),
    );
}

#[test]
fn validateaddress_renders_valid_and_invalid_addresses() {
    use zaino_address::ValidatedAddress;

    assert_golden(
        "validateaddress_invalid",
        &address::validate_address_from_domain(ValidatedAddress::Invalid),
    );
    assert_golden(
        "validateaddress_valid",
        &address::validate_address_from_domain(ValidatedAddress::Transparent {
            address: ADDRESS.to_string(),
            is_script: false,
        }),
    );
}

#[test]
fn hashes_render_in_display_order() {
    assert_golden(
        "getbestblockhash",
        &hashes::best_block_hash_from_domain(BlockHash::from(ASYMMETRIC)),
    );
    assert_golden(
        "sendrawtransaction",
        &hashes::sent_transaction_hash_from_domain(TransactionId::from(ASYMMETRIC)),
    );
}
