//! Test vector creation and validity tests, MockchainSource creation.

use core2::io::{self, Read};
use prost::Message;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::{fs::File, path::PathBuf};
use zebra_chain::serialization::ZcashDeserialize as _;

use zaino_proto::proto::compact_formats::CompactBlock;
use zebra_rpc::methods::GetAddressUtxos;

use crate::chain_index::source::test::MockchainSource;
use crate::{read_u32_le, read_u64_le, ChainBlock, CompactSize, ZainoVersionedSerialise as _};

/// Reads test data from file.
#[allow(clippy::type_complexity)]
fn read_vectors_from_file<P: AsRef<Path>>(
    base_dir: P,
) -> io::Result<(
    Vec<(
        u32,
        ChainBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    let base = base_dir.as_ref();

    // chain_blocks.dat
    let mut chain_blocks = Vec::<(u32, ChainBlock)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("chain_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };
            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            let chain = ChainBlock::from_bytes(&buf)?; // <── new
            chain_blocks.push((height, chain));
        }
    }

    // compact_blocks.dat
    let mut compact_blocks =
        Vec::<(u32, ChainBlock, CompactBlock)>::with_capacity(chain_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("compact_blocks.dat"))?);
        for (h1, chain) in chain_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch between ChainBlock and CompactBlock",
                ));
            }
            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            let compact = CompactBlock::decode(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            compact_blocks.push((h1, chain, compact));
        }
    }

    // zebra_blocks.dat
    let mut full_blocks =
        Vec::<(u32, ChainBlock, CompactBlock, zebra_chain::block::Block)>::with_capacity(
            compact_blocks.len(),
        );
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        for (h1, chain, compact) in compact_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in zcash_blocks.dat",
                ));
            }

            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;

            let zcash_block = zebra_chain::block::Block::zcash_deserialize(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            full_blocks.push((h1, chain, compact, zcash_block));
        }
    }

    // tree_roots.dat
    let mut full_data = Vec::with_capacity(full_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (h1, chain, compact, zcash_block) in full_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
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

            full_data.push((
                h1,
                chain,
                compact,
                zcash_block,
                (sapling_root, sapling_size, orchard_root, orchard_size),
            ));
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

// TODO: Remove ChainBlocks and Compact blocks as they are no longer used,
// `zebra_chain::block::block`s are used as the single source of block data.
//
// TODO: Create seperate load methods for block_data and transparent_wallet_data.
#[allow(clippy::type_complexity)]
pub(crate) fn load_test_vectors() -> io::Result<(
    Vec<(
        u32,
        ChainBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
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
        ChainBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
) -> MockchainSource {
    let (
        mut heights,
        mut chain_blocks,
        mut compact_blocks,
        mut zebra_blocks,
        mut block_roots,
        mut block_hashes,
    ) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );

    for (
        height,
        chain_block,
        compact_block,
        zebra_block,
        (sapling_root, sapling_tree_size, orchard_root, orchard_tree_size),
    ) in blockchain_data.clone()
    {
        heights.push(height);
        chain_blocks.push(chain_block.clone());
        compact_blocks.push(compact_block);
        zebra_blocks.push(Arc::new(zebra_block));

        block_roots.push((
            Some((sapling_root, sapling_tree_size)),
            Some((orchard_root, orchard_tree_size)),
        ));

        block_hashes.push(*chain_block.index().hash());
    }

    MockchainSource::new(zebra_blocks, block_roots, block_hashes)
}

#[allow(clippy::type_complexity)]
pub(crate) fn build_active_mockchain_source(
    // the input data for this function could be reduced for wider use
    // but is more simple to pass all test block data here.
    blockchain_data: Vec<(
        u32,
        ChainBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
) -> MockchainSource {
    let (
        mut heights,
        mut chain_blocks,
        mut compact_blocks,
        mut zebra_blocks,
        mut block_roots,
        mut block_hashes,
    ) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );

    for (
        height,
        chain_block,
        compact_block,
        zebra_block,
        (sapling_root, sapling_tree_size, orchard_root, orchard_tree_size),
    ) in blockchain_data.clone()
    {
        heights.push(height);
        chain_blocks.push(chain_block.clone());
        compact_blocks.push(compact_block);
        zebra_blocks.push(Arc::new(zebra_block));

        block_roots.push((
            Some((sapling_root, sapling_tree_size)),
            Some((orchard_root, orchard_tree_size)),
        ));

        block_hashes.push(*chain_block.index().hash());
    }

    MockchainSource::new_with_active_height(zebra_blocks, block_roots, block_hashes, 0)
}

// ***** Tests *****

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vectors_can_be_loaded_and_deserialised() {
    let (blocks, faucet, recipient) = load_test_vectors().unwrap();

    // Chech block data..
    assert!(
        !blocks.is_empty(),
        "expected at least one block in test-vectors"
    );
    let mut expected_height: u32 = 0;
    for (height, chain_block, compact_block, _zebra_block, _block_roots) in &blocks {
        // println!("Checking block at height {h}");

        assert_eq!(
            expected_height, *height,
            "Chain height continuity check failed at height {height}"
        );
        expected_height = *height + 1;

        let compact_block_hash = compact_block.hash.clone();
        let chain_block_hash_bytes = chain_block.hash().0.to_vec();

        assert_eq!(
            compact_block_hash, chain_block_hash_bytes,
            "Block hash check failed at height {height}"
        );

        // ChainBlock round trip check.
        let bytes = chain_block.to_bytes().unwrap();
        let reparsed = ChainBlock::from_bytes(&bytes).unwrap();
        assert_eq!(
            chain_block, &reparsed,
            "ChainBlock round-trip failed at height {height}"
        );
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
