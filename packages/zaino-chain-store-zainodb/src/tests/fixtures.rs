//! Building a store, and the chain to build it from, out of the vectors.
//!
//! Shared by this crate's finalised-state and migration suites, and by
//! `zaino-state`'s remaining suites through the `testing` feature — the block
//! chain these produce is the oracle both sides compare against, so a second
//! copy of it would be a second oracle.

use std::collections::HashMap;

pub(crate) use super::vectors::VectorBlock;

#[cfg(all(test, not(feature = "transparent_address_history_experimental")))]
pub(crate) use super::fake_validator::fake_validator_with_tip;
/// This crate's own tests only: a consumer wanting a mock validator wants one
/// shaped for its own ports, not for this crate's four.
#[cfg(test)]
pub(crate) use super::fake_validator::{fake_validator_from_vectors, FakeValidator};
use crate::types::{BlockMetadata, BlockWithMetadata, ChainWork, CompactTxData, IndexedBlock};

/// The network the vector chain was mined on.
///
/// Regtest with every upgrade through NU6 active at height 1, and nothing
/// beyond it. Named once: the block builder branches on activation heights, so
/// two call sites disagreeing about them would produce two different chains
/// from the same blocks.
pub fn vector_network() -> zebra_chain::parameters::Network {
    zebra_chain::parameters::Network::new_regtest(
        zebra_chain::parameters::testnet::ConfiguredActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(1),
            nu6: Some(1),
            // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
            nu6_1: None,
            nu6_2: None,
            nu6_3: None,
            nu7: None,
        }
        .into(),
    )
}

/// The vector chain as [`IndexedBlock`]s, with chainwork threaded between them.
///
/// The sole source of truth for materialising the chain: it builds each block's
/// metadata from the recorded roots and accumulates work across successive
/// blocks. Returned lazily so each caller picks its own consumption strategy
/// without duplicating the metadata boilerplate.
///
/// Built through `BlockWithMetadata` — the `zebra_chain` path — deliberately.
/// That is what makes it an *independent* oracle for the store's own build
/// path, which now assembles blocks from `zaino-primitives` instead.
pub fn indexed_block_chain(blocks: &[VectorBlock]) -> impl Iterator<Item = IndexedBlock> + '_ {
    let network = vector_network();
    let mut parent_chainwork: Option<ChainWork> = None;

    blocks.iter().map(move |vector| {
        let metadata = BlockMetadata {
            sapling_root: vector.sapling_root,
            sapling_size: vector.sapling_tree_size as u32,
            orchard_root: vector.orchard_root,
            orchard_size: vector.orchard_tree_size as u32,
            ironwood: None,
            parent_chainwork,
            network: network.clone(),
        };
        let block = IndexedBlock::try_from(BlockWithMetadata::new(&vector.zebra_block, metadata))
            .expect("vector blocks are valid");
        parent_chainwork = Some(block.context.chainwork);
        block
    })
}

/// The chain, plus a flat `(height, tx_index)` lookup into its transactions.
///
/// So a spender-symmetry test can walk the chain once for its outpoint scan and
/// then resolve spender references in constant time rather than re-walking.
#[allow(clippy::type_complexity)]
pub fn index_vector_blocks(
    blocks: &[VectorBlock],
) -> (Vec<IndexedBlock>, HashMap<(u32, u64), CompactTxData>) {
    let mut indexed = Vec::with_capacity(blocks.len());
    let mut by_position: HashMap<(u32, u64), CompactTxData> = HashMap::new();

    for block in indexed_block_chain(blocks) {
        let height = block.context.index.height.0;
        for tx in block.transactions() {
            by_position.insert((height, tx.index()), tx.clone());
        }
        indexed.push(block);
    }

    (indexed, by_position)
}

/// Recursively copies `src` into `dst`, creating `dst` if absent.
///
/// Used to seed a fresh tempdir from a pre-built fixture database, so each test
/// gets an isolated writable copy without paying for a fresh ingest.
pub fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The vector chain and the two wallets' recorded results.
///
/// Kept as one struct because a test asserting a wallet's txids needs the chain
/// those txids are in; handing them over separately invites a suite that checks
/// one against the other's fixture.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct TestVectorData {
    /// Every block of the chain, in height order from genesis.
    pub blocks: Vec<VectorBlock>,
    /// What the faucet wallet should see.
    pub faucet: TestVectorClientData,
    /// What the recipient wallet should see.
    pub recipient: TestVectorClientData,
}

/// One wallet's recorded view of the vector chain.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct TestVectorClientData {
    /// Transaction ids touching this wallet, hex-encoded, in chain order.
    pub txids: Vec<String>,
    /// The wallet's unspent outputs.
    pub utxos: Vec<zebra_rpc::methods::GetAddressUtxos>,
    /// The wallet's transparent balance.
    pub balance: u64,
}

/// Loads the chain and both wallets' recorded results.
#[cfg(test)]
pub fn load_test_vectors() -> corez::io::Result<TestVectorData> {
    use std::fs::File;

    let base = super::vectors::vectors_dir();
    let client_data = |file: &str| -> corez::io::Result<TestVectorClientData> {
        let (txids, utxos, balance) = serde_json::from_reader(File::open(base.join(file))?)
            .map_err(|error| corez::io::Error::new(corez::io::ErrorKind::InvalidData, error))?;
        Ok(TestVectorClientData {
            txids,
            utxos,
            balance,
        })
    };

    Ok(TestVectorData {
        blocks: super::vectors::load_vector_blocks()?,
        faucet: client_data("faucet_data.json")?,
        recipient: client_data("recipient_data.json")?,
    })
}

/// The vector chain truncated at `height_limit`, if one is given.
///
/// The shared half of the two writers below: they differ only in what they
/// hand each block to, and a second `break`-on-limit loop would be a second
/// place for the bound to drift.
fn vector_chain_up_to(
    blocks: &[VectorBlock],
    height_limit: Option<u32>,
) -> impl Iterator<Item = IndexedBlock> + '_ {
    indexed_block_chain(blocks).take_while(move |block| {
        height_limit.is_none_or(|limit| block.context.index.height.0 <= limit)
    })
}

/// Writes the vector chain into `db`, up to `height_limit` if given.
///
/// Writes through the backend's own writer rather than the store's sync path,
/// so a test can build a database at an exact height without a source.
#[cfg(test)]
pub async fn sync_db_with_blockdata(
    db: &impl crate::store::capability::DbWrite,
    blocks: &[VectorBlock],
    height_limit: Option<u32>,
) {
    for block in vector_chain_up_to(blocks, height_limit) {
        db.write_block(block).await.expect("vector block writes");
    }
}

/// Fills `store` with the vector chain, up to `height_limit` if given.
///
/// The dependant-facing counterpart to [`sync_db_with_blockdata`], which is
/// this crate's own because its backend-writer bound is crate-private. Both
/// deliberately bypass [`FinalisedState::build_to`]: a caller that only needs a
/// database *at* a height should not pay for the store's ingest machinery —
/// the batch write path spins the background validator hard enough to dominate
/// a seed build under a parallel test runner.
pub async fn fill_store_with_blockdata<T: zaino_chain_store::ChainStoreSource>(
    store: &crate::store::FinalisedState<T>,
    blocks: &[VectorBlock],
    height_limit: Option<u32>,
) {
    for block in vector_chain_up_to(blocks, height_limit) {
        store.write_block(block).await.expect("vector block writes");
    }
    store.refresh_watermark().await;
}

/// The two configs a test spawns a persistent store from, for a database at
/// `path`.
///
/// One helper rather than the pair spelled out at every call site: a store is
/// configured from a neutral half and this backend's half, and a test that
/// only cares *where* the database goes should not have to restate the other
/// nine fields to say so.
pub fn test_store_config(
    path: impl Into<std::path::PathBuf>,
) -> (
    zaino_chain_store::ChainStoreConfig,
    crate::config::ZainoDbConfig,
) {
    (
        zaino_chain_store::ChainStoreConfig::at_path(path),
        crate::config::ZainoDbConfig::new(
            zaino_common::network::ActivationHeights::default().to_regtest_network(),
        ),
    )
}
