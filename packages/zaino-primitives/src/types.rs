//! Domain types shared across the Zaino stack: chain vocabulary + peer grouping ([`Netgroup`])

mod announced;
mod block;
mod block_commitments;
mod block_ref;
mod compact_ciphertext;
mod compact_difficulty;
mod ephemeral_key;
mod equihash_solution;
mod fee;
mod finality_depth;
mod header_bytes;
mod merkle_root;
mod netgroup;
mod note_commitment;
mod nullifier;
mod peer_txid;
mod per_pool;
mod subtree_root;
mod transaction;
mod transaction_location;
mod tree_root;
mod tree_size;
mod treestate;

pub use announced::Announced;
pub use block::{Block, BodyMismatch, DecodedHeader};
pub use block_commitments::BlockCommitments;
pub use block_ref::BlockRef;
pub use compact_ciphertext::CompactCiphertext;
pub use compact_difficulty::{CompactDifficulty, CompactDifficultyError, Target};
pub use ephemeral_key::EphemeralKey;
pub use equihash_solution::EquihashSolution;
pub use fee::{BlockFees, Fee, FeeError};
pub use finality_depth::FinalityDepth;
pub use header_bytes::{HeaderBytes, HeaderError, REGTEST_SOLUTION, STANDARD_SOLUTION};
pub use merkle_root::MerkleRoot;
pub use netgroup::{Netgroup, ECLIPSE_NETGROUPS};
pub use note_commitment::NoteCommitment;
pub use nullifier::Nullifier;
pub use peer_txid::PeerTxId;
pub use per_pool::PerPool;
pub use subtree_root::SubtreeRoot;
pub use transaction::{
    OrchardAction, OrchardData, SaplingData, SaplingOutput, SaplingSpend, SproutData, Transaction,
    TransparentData, TransparentOutput,
};
pub use transaction_location::TransactionLocation;
pub use tree_root::TreeRoot;
pub use tree_size::{TreeSize, TreeSizes};
pub use treestate::{CommitmentTreeBytes, Treestate};
