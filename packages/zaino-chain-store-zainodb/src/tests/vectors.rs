//! Where the checked-in chain vectors are, and how to read the parts of them
//! this crate needs.
//!
//! # Why the data lives here
//!
//! These vectors are a regtest chain: blocks, the commitment tree roots after
//! each, the treestates, and two wallets' expected balances. Their heaviest
//! consumers are this crate's finalised-state and migration suites, which build
//! a database from them and assert on what comes back — so the data sits with
//! the code it exercises.
//!
//! `zaino-state`'s remaining suites need the same chain. Rather than a second
//! copy or a path reaching across crates, they take this crate's `testing`
//! feature and call [`vectors_dir`]. The feature is dev-dependency-only, so
//! none of this reaches a shipped build.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use corez::io::{self, Read};
use zaino_encoding::{read_u32_le, read_u64_le, CompactSize};
use zebra_chain::serialization::ZcashDeserialize as _;

/// One block of the vector chain, as it was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorBlock {
    /// Height in the chain, starting at genesis.
    pub height: u32,
    /// The block, parsed from its consensus encoding.
    pub zebra_block: zebra_chain::block::Block,
    /// Sapling commitment tree root after this block.
    pub sapling_root: zebra_chain::sapling::tree::Root,
    /// Cumulative sapling note count after this block.
    pub sapling_tree_size: u64,
    /// Serialized sapling frontier after this block.
    pub sapling_tree_state: Vec<u8>,
    /// Orchard commitment tree root after this block.
    pub orchard_root: zebra_chain::orchard::tree::Root,
    /// Cumulative orchard note count after this block.
    pub orchard_tree_size: u64,
    /// Serialized orchard frontier after this block.
    pub orchard_tree_state: Vec<u8>,
}

/// The directory holding the checked-in vector files.
///
/// Exposed so a consumer in another crate can read the parts this one does not
/// parse — the two wallet JSON files need `zebra-rpc` types, which a storage
/// crate has no reason to depend on. One function crossing the boundary rather
/// than a path literal repeated in both places.
pub fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("tests")
        .join("vectors")
}

/// Reads the vector chain: every block with its roots and treestates.
///
/// Ascending by height and contiguous from genesis; the reader checks that the
/// three files agree on the height at each step rather than trusting their
/// order, because a mismatch would silently pair a block with another block's
/// treestate.
pub fn load_vector_blocks() -> io::Result<Vec<VectorBlock>> {
    let base = vectors_dir();

    let mut zebra_blocks = Vec::<(u32, zebra_chain::block::Block)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(height) => height,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            };

            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;

            let block = zebra_chain::block::Block::zcash_deserialize(&*buf)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

            zebra_blocks.push((height, block));
        }
    }

    let mut blocks_and_roots = Vec::with_capacity(zebra_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (height, zebra_block) in zebra_blocks {
            expect_height(&mut r, height, "tree_roots.dat")?;

            let mut sapling_bytes = [0u8; 32];
            r.read_exact(&mut sapling_bytes)?;
            let sapling_root = zebra_chain::sapling::tree::Root::try_from(sapling_bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let sapling_tree_size = read_u64_le(&mut r)?;

            let mut orchard_bytes = [0u8; 32];
            r.read_exact(&mut orchard_bytes)?;
            let orchard_root = zebra_chain::orchard::tree::Root::try_from(orchard_bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let orchard_tree_size = read_u64_le(&mut r)?;

            blocks_and_roots.push((
                height,
                zebra_block,
                sapling_root,
                sapling_tree_size,
                orchard_root,
                orchard_tree_size,
            ));
        }
    }

    let mut blocks = Vec::with_capacity(blocks_and_roots.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_states.dat"))?);
        for (
            height,
            zebra_block,
            sapling_root,
            sapling_tree_size,
            orchard_root,
            orchard_tree_size,
        ) in blocks_and_roots
        {
            expect_height(&mut r, height, "tree_states.dat")?;

            let sapling_tree_state = read_sized(&mut r)?;
            let orchard_tree_state = read_sized(&mut r)?;

            blocks.push(VectorBlock {
                height,
                zebra_block,
                sapling_root,
                sapling_tree_size,
                sapling_tree_state,
                orchard_root,
                orchard_tree_size,
                orchard_tree_state,
            });
        }
    }

    Ok(blocks)
}

/// Reads a height and checks it is the one expected.
fn expect_height<R: Read>(mut r: R, expected: u32, file: &str) -> io::Result<()> {
    let found = read_u32_le(&mut r)?;
    if found != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("height mismatch in {file}: expected {expected}, found {found}"),
        ));
    }
    Ok(())
}

/// Reads a length-prefixed byte string.
fn read_sized<R: Read>(mut r: R) -> io::Result<Vec<u8>> {
    let len: usize = CompactSize::read_t(&mut r)?;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}
