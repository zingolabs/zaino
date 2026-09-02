//! Test vector creation and validity tests, MockchainSource creation.

use std::fs::File;
use std::io;
use std::sync::Arc;

use zebra_rpc::methods::GetAddressUtxos;

use crate::chain_index::source::mockchain_source::MockchainSource;
use crate::chain_index::types::BlockHash;
use crate::chain_index::validator_source::ValidatorSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestVectorData {
    pub blocks: Vec<TestVectorBlockData>,
    pub faucet: TestVectorClientData,
    pub recipient: TestVectorClientData,
}

/// One block of the vector chain.
///
/// The backend's type under this crate's old name, not a copy of it: two
/// structs with the same fields would be two things to keep in step, and the
/// suites here compare against blocks the backend built.
pub type TestVectorBlockData = zaino_chain_store_zainodb::tests::vectors::VectorBlock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestVectorClientData {
    pub txids: Vec<String>,
    pub utxos: Vec<GetAddressUtxos>,
    pub balance: u64,
}

/// The vector chain as `IndexedBlock`s.
///
/// Re-exported from `zaino-chain-store-zainodb`, which is where the vectors and
/// the block builder now live. Kept under this name so the suites that use it
/// read as they did — and shared rather than copied, because it is the oracle
/// the chain-head conversion is compared against, and two oracles is one too
/// many.
pub(crate) use zaino_chain_store_zainodb::tests::fixtures::{
    copy_dir_recursive, indexed_block_chain,
};

// TODO: Add custom MockChain block data structs to simplify unit test interface
// and add getter methods for comonly used types.
/// Reads the vector chain and the two wallets' expected data.
///
/// The chain itself is read by `zaino-chain-store-zainodb`, which is where the
/// files live: its finalised-state and migration suites are their heaviest
/// consumers. This adds the two wallet JSON files, which need `zebra-rpc` types
/// that a storage crate has no reason to depend on.
pub(crate) fn read_vectors_from_file() -> io::Result<TestVectorData> {
    let base = zaino_chain_store_zainodb::tests::vectors::vectors_dir();

    let blocks = zaino_chain_store_zainodb::tests::vectors::load_vector_blocks()?;

    let client_data = |file: &str| -> io::Result<TestVectorClientData> {
        let (txids, utxos, balance) = serde_json::from_reader(File::open(base.join(file))?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(TestVectorClientData {
            txids,
            utxos,
            balance,
        })
    };

    Ok(TestVectorData {
        blocks,
        faucet: client_data("faucet_data.json")?,
        recipient: client_data("recipient_data.json")?,
    })
}

// TODO: Remove IndexedBlocks and Compact blocks as they are no longer used,
// `zebra_chain::block::block`s are used as the single source of block data.
//
// TODO: Create seperate load methods for block_data and transparent_wallet_data.
#[allow(clippy::type_complexity)]
pub(crate) fn load_test_vectors() -> io::Result<TestVectorData> {
    read_vectors_from_file()
}

/// The mock as ChainIndex consumes it.
pub(crate) type MockSource = ValidatorSource<MockchainSource>;

/// Present the mock through ChainIndex's driven port, exactly as a validator
/// is presented: the same `ValidatorSource` conversion runs in tests and in
/// production, so these suites exercise it rather than a parallel copy of it.
fn wrap(source: MockchainSource) -> MockSource {
    ValidatorSource::new(
        source,
        crate::chain_index::source::mockchain_source::mockchain_network(),
        // The mock has no zebra state service, so no `ChainTipChange` stream —
        // the same as an RPC-only deployment.
        None,
    )
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_mockchain_source(
    // the input data for this function could be reduced for wider use
    // but is more simple to pass all test block data here.
    blockchain_data: Vec<TestVectorBlockData>,
) -> MockSource {
    let (mut heights, mut zebra_blocks, mut block_roots, mut block_hashes, mut block_treestates) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for block in blockchain_data.clone() {
        heights.push(block.height);
        block_hashes.push(BlockHash::from(block.zebra_block.hash()));
        zebra_blocks.push(Arc::new(block.zebra_block));

        block_roots.push((
            Some((block.sapling_root, block.sapling_tree_size)),
            Some((block.orchard_root, block.orchard_tree_size)),
        ));

        block_treestates.push((block.sapling_tree_state, block.orchard_tree_state));
    }

    wrap(MockchainSource::new(
        zebra_blocks,
        block_roots,
        block_treestates,
        block_hashes,
    ))
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_active_mockchain_source(
    loaded_chain_height: u32,
    // the input data for this function could be reduced for wider use
    // but is more simple to pass all test block data here.
    blockchain_data: Vec<TestVectorBlockData>,
) -> MockSource {
    let (mut heights, mut zebra_blocks, mut block_roots, mut block_hashes, mut block_treestates) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for TestVectorBlockData {
        height,
        zebra_block,
        sapling_root,
        sapling_tree_size,
        sapling_tree_state,
        orchard_root,
        orchard_tree_size,
        orchard_tree_state,
    } in blockchain_data.clone()
    {
        heights.push(height);
        block_hashes.push(BlockHash::from(zebra_block.hash()));
        zebra_blocks.push(Arc::new(zebra_block));

        block_roots.push((
            Some((sapling_root, sapling_tree_size)),
            Some((orchard_root, orchard_tree_size)),
        ));

        block_treestates.push((sapling_tree_state, orchard_tree_state));
    }

    wrap(MockchainSource::new_with_active_height(
        zebra_blocks,
        block_roots,
        block_treestates,
        block_hashes,
        loaded_chain_height,
    ))
}

// ***** Tests *****

#[tokio::test(flavor = "multi_thread")]
async fn vectors_can_be_loaded_and_deserialised() {
    let TestVectorData {
        blocks,
        faucet,
        recipient,
    } = load_test_vectors().unwrap();

    // Chech block data..
    assert!(
        !blocks.is_empty(),
        "expected at least one block in test-vectors"
    );
    let mut expected_height: u32 = 0;
    for TestVectorBlockData { height, .. } in &blocks {
        // println!("Checking block at height {h}");

        assert_eq!(
            expected_height, *height,
            "Chain height continuity check failed at height {height}"
        );
        expected_height = *height + 1;
    }

    // check taddrs.

    println!("\nFaucet UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = faucet.utxos[0].into_parts();
    println!("addr: {addr}");

    println!("\nRecipient UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = recipient.utxos[0].into_parts();
    println!("addr: {addr}");
}
