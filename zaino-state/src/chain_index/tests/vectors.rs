//! Test vector creation and validity tests, MockchainSource creation.

use core2::io::{self, Read};
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::{fs::File, path::PathBuf};
use zebra_chain::serialization::ZcashDeserialize as _;

use zebra_rpc::methods::GetAddressUtxos;

use crate::chain_index::source::test::MockchainSource;
use crate::{read_u32_le, read_u64_le, BlockHash, CompactSize};

// TODO: Add custom MockChain block data structs to simplify unit test interface
// and add getter methods for comonly used types.
#[allow(clippy::type_complexity)]
pub fn read_vectors_from_file<P: AsRef<Path>>(
    base_dir: P,
) -> io::Result<(
    Vec<(
        u32,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    let base = base_dir.as_ref();

    // zebra_blocks.dat
    let mut zebra_blocks = Vec::<(u32, zebra_chain::block::Block)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };

            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;

            let zcash_block = zebra_chain::block::Block::zcash_deserialize(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            zebra_blocks.push((height, zcash_block));
        }
    }

    // tree_roots.dat
    let mut blocks_and_roots = Vec::with_capacity(zebra_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (height, zebra_block) in zebra_blocks {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_roots.dat",
                ));
            }
            let mut sapling_bytes = [0u8; 32];
            r.read_exact(&mut sapling_bytes)?;
            let sapling_root = zebra_chain::sapling::tree::Root::try_from(sapling_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let sapling_size = read_u64_le(&mut r)?;

            let mut orchard_bytes = [0u8; 32];
            r.read_exact(&mut orchard_bytes)?;
            let orchard_root = zebra_chain::orchard::tree::Root::try_from(orchard_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let orchard_size = read_u64_le(&mut r)?;

            blocks_and_roots.push((
                height,
                zebra_block,
                (sapling_root, sapling_size, orchard_root, orchard_size),
            ));
        }
    }

    // tree_states.dat
    let mut full_data = Vec::with_capacity(blocks_and_roots.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_states.dat"))?);
        for (height, zebra_block, roots) in blocks_and_roots {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_states.dat",
                ));
            }

            let sapling_len: usize = CompactSize::read_t(&mut r)?;
            let mut sapling_state = vec![0u8; sapling_len];
            r.read_exact(&mut sapling_state)?;

            let orchard_len: usize = CompactSize::read_t(&mut r)?;
            let mut orchard_state = vec![0u8; orchard_len];
            r.read_exact(&mut orchard_state)?;

            full_data.push((height, zebra_block, roots, (sapling_state, orchard_state)));
        }
    }

    // faucet_data.json
    let faucet = serde_json::from_reader(File::open(base.join("faucet_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // recipient_data.json
    let recipient = serde_json::from_reader(File::open(base.join("recipient_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok((full_data, faucet, recipient))
}

// TODO: Remove IndexedBlocks and Compact blocks as they are no longer used,
// `zebra_chain::block::block`s are used as the single source of block data.
//
// TODO: Create seperate load methods for block_data and transparent_wallet_data.
#[allow(clippy::type_complexity)]
pub(crate) fn load_test_vectors() -> io::Result<(
    Vec<(
        u32,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    // <repo>/zaino-state/src/chain_index/tests/vectors
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("chain_index")
        .join("tests")
        .join("vectors");
    read_vectors_from_file(&base_dir)
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_mockchain_source(
    // the input data for this function could be reduced for wider use
    // but is more simple to pass all test block data here.
    blockchain_data: Vec<(
        u32,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )>,
) -> MockchainSource {
    let (mut heights, mut zebra_blocks, mut block_roots, mut block_hashes, mut block_treestates) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for (
        height,
        zebra_block,
        (sapling_root, sapling_tree_size, orchard_root, orchard_tree_size),
        (sapling_treestate, orchard_treestate),
    ) in blockchain_data.clone()
    {
        heights.push(height);
        block_hashes.push(BlockHash::from(zebra_block.hash()));
        zebra_blocks.push(Arc::new(zebra_block));

        block_roots.push((
            Some((sapling_root, sapling_tree_size)),
            Some((orchard_root, orchard_tree_size)),
        ));

        block_treestates.push((sapling_treestate, orchard_treestate));
    }

    MockchainSource::new(zebra_blocks, block_roots, block_treestates, block_hashes)
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_active_mockchain_source(
    loaded_chain_height: u32,
    // the input data for this function could be reduced for wider use
    // but is more simple to pass all test block data here.
    blockchain_data: Vec<(
        u32,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )>,
) -> MockchainSource {
    let (mut heights, mut zebra_blocks, mut block_roots, mut block_hashes, mut block_treestates) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for (
        height,
        zebra_block,
        (sapling_root, sapling_tree_size, orchard_root, orchard_tree_size),
        (sapling_treestate, orchard_treestate),
    ) in blockchain_data.clone()
    {
        heights.push(height);
        block_hashes.push(BlockHash::from(zebra_block.hash()));
        zebra_blocks.push(Arc::new(zebra_block));

        block_roots.push((
            Some((sapling_root, sapling_tree_size)),
            Some((orchard_root, orchard_tree_size)),
        ));

        block_treestates.push((sapling_treestate, orchard_treestate));
    }

    MockchainSource::new_with_active_height(
        zebra_blocks,
        block_roots,
        block_treestates,
        block_hashes,
        loaded_chain_height,
    )
}

// ***** Tests *****

#[tokio::test(flavor = "multi_thread")]
async fn vectors_can_be_loaded_and_deserialised() {
    let (blocks, faucet, recipient) = load_test_vectors().unwrap();

    // Chech block data..
    assert!(
        !blocks.is_empty(),
        "expected at least one block in test-vectors"
    );
    let mut expected_height: u32 = 0;
    for (height, _zebra_block, _block_roots, _block_treestate) in &blocks {
        // println!("Checking block at height {h}");

        assert_eq!(
            expected_height, *height,
            "Chain height continuity check failed at height {height}"
        );
        expected_height = *height + 1;
    }

    // check taddrs.
    let (_, utxos_f, _) = faucet;
    let (_, utxos_r, _) = recipient;

    println!("\nFaucet UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = utxos_f[0].into_parts();
    println!("addr: {addr}");

    println!("\nRecipient UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = utxos_r[0].into_parts();
    println!("addr: {addr}");
}
