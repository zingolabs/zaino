//! Test vector creation and validity tests, MockchainSource creation.

use std::collections::HashMap;
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

/// Tip of the frozen corpus. Every consuming suite's height arithmetic
/// (`finalized_height_floor(200) = 100`, the 150-block active-height fixtures)
/// is derived from this, so a truncated corpus must fail loudly here rather
/// than silently shorten every other test's chain.
const CORPUS_TIP_HEIGHT: u32 = 200;

#[test]
fn vectors_can_be_loaded_and_deserialised() {
    let TestVectorData { blocks, .. } = load_test_vectors().unwrap();

    assert_eq!(
        blocks.len() as u32,
        CORPUS_TIP_HEIGHT + 1,
        "corpus must be the genesis-rooted {}-block chain the consuming suites assume",
        CORPUS_TIP_HEIGHT + 1
    );

    for (expected_height, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height, expected_height as u32,
            "chain height continuity check failed at index {expected_height}"
        );
        assert!(
            !block.sapling_tree_state.is_empty() && !block.orchard_tree_state.is_empty(),
            "block {} carries an empty treestate blob; `get_tree_state` parity tests would \
             assert nothing",
            block.height
        );
    }

    for pair in blocks.windows(2) {
        let (parent, child) = (&pair[0], &pair[1]);
        assert_eq!(
            child.zebra_block.header.previous_block_hash,
            parent.zebra_block.hash(),
            "block {} does not link to its parent — the corpus is not one chain",
            child.height
        );

        // An append-only note commitment tree changes its root exactly when it
        // grows, so a root that moves without the size moving (or vice versa)
        // means the roots and the blocks came from different captures.
        assert!(
            child.sapling_tree_size >= parent.sapling_tree_size,
            "sapling tree shrank at height {}",
            child.height
        );
        assert_eq!(
            child.sapling_root != parent.sapling_root,
            child.sapling_tree_size != parent.sapling_tree_size,
            "sapling root and size disagree about growth at height {}",
            child.height
        );
        assert!(
            child.orchard_tree_size >= parent.orchard_tree_size,
            "orchard tree shrank at height {}",
            child.height
        );
        assert_eq!(
            child.orchard_root != parent.orchard_root,
            child.orchard_tree_size != parent.orchard_tree_size,
            "orchard root and size disagree about growth at height {}",
            child.height
        );
    }

    let tip = blocks.last().expect("corpus length asserted above");
    assert!(
        tip.sapling_tree_size > 0 && tip.orchard_tree_size > 0,
        "corpus must carry both sapling and orchard activity (sapling {}, orchard {}) — the \
         mixed-pool shape is what the compact-block and treestate suites read it for",
        tip.sapling_tree_size,
        tip.orchard_tree_size
    );
}

#[test]
fn wallet_vectors_agree_with_chain() {
    let TestVectorData {
        blocks,
        faucet,
        recipient,
    } = load_test_vectors().unwrap();

    let mut txids_by_height: HashMap<u32, Vec<String>> = HashMap::new();
    for block in &blocks {
        txids_by_height.insert(
            block.height,
            block
                .zebra_block
                .transactions
                .iter()
                .map(|tx| tx.hash().to_string())
                .collect(),
        );
    }

    let faucet_address = assert_wallet_vector_consistent("faucet", &faucet, &txids_by_height);
    let recipient_address =
        assert_wallet_vector_consistent("recipient", &recipient, &txids_by_height);

    assert_ne!(
        faucet_address, recipient_address,
        "the two wallet vectors must describe different addresses"
    );
}

/// Asserts one wallet's `(txids, utxos, balance)` triple is internally coherent
/// and anchored in the block vectors, returning the single address it covers.
fn assert_wallet_vector_consistent(
    label: &str,
    client: &TestVectorClientData,
    txids_by_height: &HashMap<u32, Vec<String>>,
) -> String {
    assert!(
        !client.txids.is_empty(),
        "{label} txid vector is empty; the address-history suites would pass vacuously"
    );
    assert!(
        !client.utxos.is_empty(),
        "{label} utxo vector is empty; the utxo suites would pass vacuously"
    );

    let mut addresses = std::collections::HashSet::new();
    let mut total: u64 = 0;
    for utxo in &client.utxos {
        let (address, txid, _output_index, _script, satoshis, height) = utxo.into_parts();
        addresses.insert(address.to_string());
        total = total
            .checked_add(satoshis)
            .expect("corpus utxo values must not overflow a u64 balance");

        let txid = txid.to_string();
        assert!(
            client.txids.contains(&txid),
            "{label} utxo {txid} is absent from the same wallet's txid vector"
        );
        let block_txids = txids_by_height.get(&height.0).unwrap_or_else(|| {
            panic!(
                "{label} utxo {txid} names height {}, outside the corpus",
                height.0
            )
        });
        assert!(
            block_txids.contains(&txid),
            "{label} utxo {txid} is not in block {} — the wallet and block vectors are from \
             different captures",
            height.0
        );
    }

    assert_eq!(
        client.balance, total,
        "{label} balance must equal the sum of its utxos"
    );
    assert_eq!(
        addresses.len(),
        1,
        "{label} vectors must cover exactly one transparent address, found {addresses:?}"
    );

    addresses
        .into_iter()
        .next()
        .expect("single-address invariant asserted above")
}
