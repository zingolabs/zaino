//! FlyClient MMR tree implementation for chain history proofs (ZIP-221/ZIP-307).
//!
//! This module builds and maintains an in-memory Merkle Mountain Range (MMR) tree
//! from data already stored in Zaino's LMDB database. The tree resets at each
//! network upgrade epoch.
//!
//! The MMR is used to generate `ChainProof` responses for the `GetChainProof` gRPC,
//! enabling light clients to probabilistically verify they are on the correct chain
//! without trusting the server.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};
use zcash_history::{Entry, EntryLink, Tree, Version, V2};
use zebra_chain::parameters::{Network, NetworkUpgrade};

use crate::chain_index::finalised_state::reader::DbReader;
use crate::chain_index::types::db::legacy::{BlockHeaderData, ChainWork};
use crate::chain_index::types::Height;
use zaino_proto::proto::service::MmrNode;

/// Errors that can occur during MMR operations.
#[derive(Debug, thiserror::Error)]
pub enum MmrError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("MMR tree error: {0}")]
    Tree(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("MMR is empty")]
    Empty,
}

impl From<zcash_history::Error> for MmrError {
    fn from(e: zcash_history::Error) -> Self {
        MmrError::Tree(e.to_string())
    }
}

/// In-memory MMR tree for a single network upgrade epoch.
///
/// Stores all MMR entries as serialized bytes. The tree is built from block headers
/// and commitment tree data already in LMDB, and maintained incrementally.
///
/// The tree resets at each network upgrade. Memory usage is ~500 bytes per entry
/// with ~2 entries per block (leaves + internal nodes). For an epoch spanning
/// 131K blocks this is ~130 MB; for 5M blocks ~1-2 GB.
#[derive(Debug)]
pub struct InMemoryMmrTree {
    /// All MMR entries in flat array order (serialized). Index = MMR position.
    entries: Vec<Vec<u8>>,
    /// Number of leaves (= number of blocks in epoch).
    leaf_count: u32,
    /// Epoch start height (network upgrade activation).
    epoch_start_height: u32,
    /// Consensus branch ID for this epoch.
    branch_id: u32,
}

impl InMemoryMmrTree {
    /// Build the MMR tree from LMDB data for the current epoch.
    pub async fn build_from_db(
        db: &DbReader,
        network: &Network,
        epoch_start: Height,
        tip_height: Height,
    ) -> Result<Self, MmrError> {
        let branch_id = branch_id_for_height(network, epoch_start)?;

        if tip_height.0 < epoch_start.0 {
            return Ok(Self {
                entries: Vec::new(),
                leaf_count: 0,
                epoch_start_height: epoch_start.0,
                branch_id,
            });
        }

        let num_blocks = (tip_height.0 - epoch_start.0 + 1) as usize;
        info!(
            "Building MMR tree for epoch starting at height {} with {} blocks",
            epoch_start.0, num_blocks
        );

        let headers = db
            .get_block_range_headers(epoch_start, tip_height)
            .await
            .map_err(|e| MmrError::Database(e.to_string()))?;
        let commitments = db
            .get_block_range_commitment_tree_data(epoch_start, tip_height)
            .await
            .map_err(|e| MmrError::Database(e.to_string()))?;

        if headers.len() != num_blocks || commitments.len() != num_blocks {
            return Err(MmrError::Database(format!(
                "Expected {} headers and commitments, got {} and {}",
                num_blocks,
                headers.len(),
                commitments.len()
            )));
        }

        let mut mmr = Self {
            entries: Vec::with_capacity(num_blocks * 2),
            leaf_count: 0,
            epoch_start_height: epoch_start.0,
            branch_id,
        };

        if num_blocks == 0 {
            return Ok(mmr);
        }

        let mut prev_chainwork: Option<primitive_types::U256> = None;

        for i in 0..num_blocks {
            let header = &headers[i];
            let commitment = &commitments[i];
            let height = epoch_start.0 + i as u32;

            let current_chainwork = header.index().chainwork().to_u256();
            let block_work = if let Some(prev) = prev_chainwork {
                current_chainwork.saturating_sub(prev)
            } else {
                work_from_nbits(header.data().bits())
            };
            prev_chainwork = Some(current_chainwork);

            let mut work_bytes = [0u8; 32];
            work_bytes = block_work.to_little_endian();

            let node_data_bytes = serialize_v2_leaf_data(
                branch_id,
                &header.index().hash().0,
                header.data().time() as u32,
                header.data().bits(),
                commitment.roots().sapling(),
                commitment.roots().orchard(),
                &work_bytes,
                height as u64,
            );

            mmr.append_leaf_bytes(&node_data_bytes)?;

            if (i + 1) % 10000 == 0 {
                debug!("Built MMR for {} / {} blocks", i + 1, num_blocks);
            }
        }

        info!(
            "MMR tree built: {} leaves, {} total entries",
            mmr.leaf_count,
            mmr.entries.len()
        );

        Ok(mmr)
    }

    /// Append a single leaf from serialized V2 node data bytes.
    fn append_leaf_bytes(&mut self, node_data_bytes: &[u8]) -> Result<(), MmrError> {
        let node_data = V2::from_bytes(self.branch_id, node_data_bytes).map_err(MmrError::Io)?;

        if self.leaf_count == 0 {
            // First leaf: just store it, no tree needed yet
            let leaf_entry = Entry::<V2>::new_leaf(node_data);
            let mut buf = Vec::new();
            leaf_entry.write(&mut buf)?;
            self.entries.push(buf);
            self.leaf_count = 1;
            return Ok(());
        }

        // For leaf_count >= 1, reconstruct tree from current peaks and append
        let peaks = find_peaks(self.entries.len() as u32);
        let peak_entries: Vec<(u32, Entry<V2>)> = peaks
            .iter()
            .map(|&pos| {
                let entry = Entry::from_bytes(self.branch_id, &self.entries[pos as usize])
                    .map_err(MmrError::Io)?;
                Ok((pos, entry))
            })
            .collect::<Result<Vec<_>, MmrError>>()?;

        let mut tree = Tree::new(self.entries.len() as u32, peak_entries, vec![]);
        let appended = tree.append_leaf(node_data)?;

        for link in &appended {
            if let EntryLink::Stored(pos) = link {
                let indexed = tree
                    .resolve_link(*link)
                    .map_err(|e| MmrError::Tree(e.to_string()))?;
                let mut buf = Vec::new();
                indexed.node().write(&mut buf)?;
                let pos = *pos as usize;
                while self.entries.len() <= pos {
                    self.entries.push(Vec::new());
                }
                self.entries[pos] = buf;
            }
        }

        self.leaf_count += 1;
        Ok(())
    }

    /// Append a new block to the MMR.
    pub fn append_block(
        &mut self,
        header: &BlockHeaderData,
        commitment: &crate::chain_index::types::db::commitment::CommitmentTreeData,
        prev_chainwork: Option<&ChainWork>,
    ) -> Result<(), MmrError> {
        let height = header.index().height().0;
        let current_chainwork = header.index().chainwork().to_u256();
        let block_work = match prev_chainwork {
            Some(prev) => current_chainwork.saturating_sub(prev.to_u256()),
            None => work_from_nbits(header.data().bits()),
        };

        let work_bytes = block_work.to_little_endian();

        let node_data_bytes = serialize_v2_leaf_data(
            self.branch_id,
            &header.index().hash().0,
            header.data().time() as u32,
            header.data().bits(),
            commitment.roots().sapling(),
            commitment.roots().orchard(),
            &work_bytes,
            height as u64,
        );

        self.append_leaf_bytes(&node_data_bytes)
    }

    /// Truncate the last leaf from the tree (for reorg handling).
    pub fn truncate_leaf(&mut self) -> Result<(), MmrError> {
        if self.leaf_count == 0 {
            return Err(MmrError::Empty);
        }
        if self.leaf_count == 1 {
            self.entries.clear();
            self.leaf_count = 0;
            return Ok(());
        }
        if self.leaf_count == 2 {
            self.entries.truncate(1);
            self.leaf_count = 1;
            return Ok(());
        }

        let peaks = find_peaks(self.entries.len() as u32);
        let peak_entries: Vec<(u32, Entry<V2>)> = peaks
            .iter()
            .map(|&pos| {
                let entry = Entry::from_bytes(self.branch_id, &self.entries[pos as usize])
                    .map_err(MmrError::Io)?;
                Ok((pos, entry))
            })
            .collect::<Result<Vec<_>, MmrError>>()?;

        let mut tree = Tree::new(self.entries.len() as u32, peak_entries, vec![]);
        let truncated = tree.truncate_leaf()?;

        for _ in 0..truncated {
            self.entries.pop();
        }

        self.leaf_count -= 1;
        Ok(())
    }

    /// Compute the MMR root hash.
    pub fn root_hash(&self) -> Result<[u8; 32], MmrError> {
        if self.leaf_count == 0 {
            return Err(MmrError::Empty);
        }

        let peaks = find_peaks(self.entries.len() as u32);
        let peak_entries: Vec<(u32, Entry<V2>)> = peaks
            .iter()
            .map(|&pos| {
                let entry = Entry::from_bytes(self.branch_id, &self.entries[pos as usize])
                    .map_err(MmrError::Io)?;
                Ok((pos, entry))
            })
            .collect::<Result<Vec<_>, MmrError>>()?;

        let tree = Tree::new(self.entries.len() as u32, peak_entries, vec![]);
        let root = tree
            .root_node()
            .map_err(|e| MmrError::Tree(e.to_string()))?;

        Ok(V2::hash(root.data()))
    }

    /// Generate an inclusion proof for the block at the given height.
    ///
    /// Returns (leaf, siblings) — the caller combines these with the MMR root
    /// and auth_data_root to assemble a `BlockInclusionProof`.
    pub fn prove_inclusion(&self, block_height: u32) -> Result<(MmrNode, Vec<MmrNode>), MmrError> {
        if block_height < self.epoch_start_height {
            return Err(MmrError::Tree(format!(
                "Block {} before epoch start {}",
                block_height, self.epoch_start_height
            )));
        }

        let leaf_index = block_height - self.epoch_start_height;
        if leaf_index >= self.leaf_count {
            return Err(MmrError::Tree(format!(
                "Block {} beyond tip (leaf_index={}, leaf_count={})",
                block_height, leaf_index, self.leaf_count
            )));
        }

        let leaf_pos = leaf_index_to_mmr_pos(leaf_index);

        let leaf_node = MmrNode {
            position: leaf_pos,
            data: self.entries[leaf_pos as usize].clone(),
        };

        let siblings = self.collect_proof_siblings(leaf_pos)?;

        Ok((leaf_node, siblings))
    }

    /// Collect sibling nodes along the path from a leaf to the root.
    fn collect_proof_siblings(&self, leaf_pos: u32) -> Result<Vec<MmrNode>, MmrError> {
        let mut siblings = Vec::new();
        let total = self.entries.len() as u32;

        if total == 0 {
            return Err(MmrError::Empty);
        }

        let mut pos = leaf_pos;
        let mut h = 0u32;

        loop {
            // Size of a complete subtree at current height
            let sibling_offset = 1u32 << h;
            let parent_span = (1u32 << (h + 1)) - 1;

            // Try: we are left child, sibling is to the right
            let right_sib = pos + sibling_offset;
            let parent = pos + parent_span + 1;

            if right_sib < total && parent <= total && pos_height(right_sib) == h {
                siblings.push(MmrNode {
                    position: right_sib,
                    data: self.entries[right_sib as usize].clone(),
                });
                pos = parent;
                h += 1;
                continue;
            }

            // Try: we are right child, sibling is to the left
            if pos >= sibling_offset {
                let left_sib = pos - sibling_offset;
                if pos_height(left_sib) == h && pos + 1 <= total {
                    siblings.push(MmrNode {
                        position: left_sib,
                        data: self.entries[left_sib as usize].clone(),
                    });
                    pos = pos + 1;
                    h += 1;
                    continue;
                }
            }

            break; // at a peak
        }

        Ok(siblings)
    }

    /// Get the number of leaves.
    pub fn leaf_count(&self) -> u32 {
        self.leaf_count
    }

    /// Get the epoch start height.
    pub fn epoch_start_height(&self) -> u32 {
        self.epoch_start_height
    }

    /// Current tip height covered by the MMR, or None if empty.
    pub fn tip_height(&self) -> Option<u32> {
        if self.leaf_count == 0 {
            None
        } else {
            Some(self.epoch_start_height + self.leaf_count - 1)
        }
    }
}

/// Push-based MMR update called from the sync loop after finalized blocks are written.
///
/// Compares the MMR tip with the current DB height and:
/// - Builds from scratch on first call or epoch change
/// - Appends new blocks when the DB has grown
/// - Truncates leaves when the DB has shrunk (reorg)
pub async fn update_mmr_after_sync(mmr: &MmrHandle, db: &DbReader, network: &Network) {
    let db_height = match db.db_height().await {
        Ok(Some(h)) => h,
        _ => return,
    };

    let mut guard = mmr.write().await;

    // First-time initialization
    if guard.is_none() {
        let epoch_start = current_epoch_start(network, db_height.0);
        match InMemoryMmrTree::build_from_db(db, network, Height(epoch_start), db_height).await {
            Ok(tree) => {
                info!(
                    "MMR tree initial build: {} leaves (heights {}..={})",
                    tree.leaf_count(),
                    epoch_start,
                    db_height.0
                );
                *guard = Some(tree);
            }
            Err(e) => {
                tracing::error!("Failed to build MMR tree: {e}");
            }
        }
        return;
    }

    let tree = guard.as_mut().unwrap();
    let mmr_tip = match tree.tip_height() {
        Some(h) => h,
        None => return,
    };

    // Epoch boundary: rebuild if the network upgrade changed
    let current_epoch = current_epoch_start(network, db_height.0);
    if current_epoch != tree.epoch_start_height {
        info!(
            "MMR epoch changed ({} -> {}), rebuilding",
            tree.epoch_start_height, current_epoch
        );
        match InMemoryMmrTree::build_from_db(db, network, Height(current_epoch), db_height).await {
            Ok(new_tree) => {
                info!(
                    "MMR rebuilt for new epoch: {} leaves",
                    new_tree.leaf_count()
                );
                *guard = Some(new_tree);
            }
            Err(e) => {
                tracing::error!("Failed to rebuild MMR for new epoch: {e}");
            }
        }
        return;
    }

    // Already up to date
    if db_height.0 == mmr_tip {
        return;
    }

    // Reorg: DB is behind MMR tip — truncate
    if db_height.0 < mmr_tip {
        let to_remove = mmr_tip - db_height.0;
        info!(
            "MMR reorg: truncating {to_remove} blocks (tip {mmr_tip} -> {})",
            db_height.0
        );
        for _ in 0..to_remove {
            if let Err(e) = tree.truncate_leaf() {
                tracing::error!("MMR truncate failed: {e}");
                break;
            }
        }
        return;
    }

    // Normal case: append new blocks
    let start = Height(mmr_tip + 1);
    let end = db_height;

    let (headers, commitments) = match (
        db.get_block_range_headers(start, end).await,
        db.get_block_range_commitment_tree_data(start, end).await,
    ) {
        (Ok(h), Ok(c)) if h.len() == c.len() => (h, c),
        _ => {
            tracing::warn!("MMR update: failed to fetch new block data");
            return;
        }
    };

    // Previous block's chainwork for computing per-block work
    let prev_chainwork = match db.get_block_header(Height(mmr_tip)).await {
        Ok(h) => Some(*h.index().chainwork()),
        Err(_) => None,
    };

    let mut prev_cw = prev_chainwork;
    let mut appended = 0u32;
    for (header, commitment) in headers.iter().zip(commitments.iter()) {
        if let Err(e) = tree.append_block(header, commitment, prev_cw.as_ref()) {
            tracing::error!("MMR append failed: {e}");
            break;
        }
        prev_cw = Some(*header.index().chainwork());
        appended += 1;
    }

    if appended > 0 {
        debug!(
            "MMR updated: +{appended} blocks (tip now {})",
            tree.tip_height().unwrap_or(0)
        );
    }
}

// ============================================================================
// Serialization helpers
// ============================================================================

/// Serialize V2 leaf node data into the zcash_history wire format.
fn serialize_v2_leaf_data(
    _branch_id: u32,
    block_hash: &[u8; 32],
    time: u32,
    bits: u32,
    sapling_root: &[u8; 32],
    orchard_root: &[u8; 32],
    work_le: &[u8; 32],
    height: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    // V1 NodeData
    buf.extend_from_slice(block_hash); // subtree_commitment
    buf.extend_from_slice(&time.to_le_bytes()); // start_time
    buf.extend_from_slice(&time.to_le_bytes()); // end_time
    buf.extend_from_slice(&bits.to_le_bytes()); // start_target
    buf.extend_from_slice(&bits.to_le_bytes()); // end_target
    buf.extend_from_slice(sapling_root); // start_sapling_root
    buf.extend_from_slice(sapling_root); // end_sapling_root
    buf.extend_from_slice(work_le); // subtree_total_work
    write_compact_uint(&mut buf, height); // start_height
    write_compact_uint(&mut buf, height); // end_height
    write_compact_uint(&mut buf, 0); // sapling_tx
                                     // V2 extra fields
    buf.extend_from_slice(orchard_root); // start_orchard_root
    buf.extend_from_slice(orchard_root); // end_orchard_root
    write_compact_uint(&mut buf, 0); // orchard_tx
    buf
}

/// Write a Bitcoin-style CompactSize uint.
fn write_compact_uint(buf: &mut Vec<u8>, n: u64) {
    if n <= 0xfc {
        buf.push(n as u8);
    } else if n <= 0xffff {
        buf.push(0xfd);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= 0xffff_ffff {
        buf.push(0xfe);
        buf.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        buf.push(0xff);
        buf.extend_from_slice(&n.to_le_bytes());
    }
}

// ============================================================================
// MMR position arithmetic
// ============================================================================

/// Height of a node at a given MMR position.
///
/// Uses the property that in the flat MMR representation, the height of position p
/// can be determined by looking at the binary representation of (p+1).
fn pos_height(pos: u32) -> u32 {
    let mut p = pos;
    let mut h = 0u32;
    loop {
        // Size of complete subtree at height h
        let size = (1u32 << (h + 1)) - 1;
        if p < size {
            return h;
        }
        // Check if p is exactly at the root of a subtree of this height
        if p == size - 1 {
            return h;
        }
        // Skip complete subtrees
        if p >= size {
            p -= size;
            h = 0;
            continue;
        }
        break;
    }
    h
}

/// Convert a 0-based leaf index to an MMR position.
fn leaf_index_to_mmr_pos(leaf_index: u32) -> u32 {
    2 * leaf_index - leaf_index.count_ones()
}

/// Find the positions of peaks in an MMR with `node_count` total nodes.
fn find_peaks(node_count: u32) -> Vec<u32> {
    if node_count == 0 {
        return vec![];
    }

    let mut peaks = Vec::new();
    let mut remaining = node_count;
    let mut offset = 0u32;

    loop {
        if remaining == 0 {
            break;
        }
        let mut h = 0u32;
        while (1u32 << (h + 2)) - 1 <= remaining {
            h += 1;
        }
        let subtree_size = (1u32 << (h + 1)) - 1;
        if subtree_size > remaining {
            break;
        }
        peaks.push(offset + subtree_size - 1);
        offset += subtree_size;
        remaining -= subtree_size;
    }

    peaks
}

// ============================================================================
// Utility functions
// ============================================================================

fn work_from_nbits(nbits: u32) -> primitive_types::U256 {
    let target = nbits_to_target(nbits);
    if target.is_zero() {
        return primitive_types::U256::zero();
    }
    let one = primitive_types::U256::one();
    (primitive_types::U256::MAX - target) / (target + one) + one
}

fn nbits_to_target(nbits: u32) -> primitive_types::U256 {
    let exp = (nbits >> 24) as usize;
    let mantissa = nbits & 0x007f_ffff;
    if exp <= 3 {
        primitive_types::U256::from(mantissa >> (8 * (3 - exp)))
    } else {
        primitive_types::U256::from(mantissa) << (8 * (exp - 3))
    }
}

fn branch_id_for_height(network: &Network, height: Height) -> Result<u32, MmrError> {
    use zebra_chain::parameters::ConsensusBranchId;
    ConsensusBranchId::current(network, zebra_chain::block::Height(height.0))
        .map(u32::from)
        .ok_or_else(|| MmrError::Tree(format!("No branch ID for height {}", height.0)))
}

/// Determine the current epoch start height.
pub fn current_epoch_start(network: &Network, tip_height: u32) -> u32 {
    let current_nu = NetworkUpgrade::current(network, zebra_chain::block::Height(tip_height));
    current_nu
        .activation_height(network)
        .map(|h| h.0)
        .unwrap_or(0)
}

/// Serialize a block header into its full wire format.
pub fn serialize_block_header(header: &BlockHeaderData) -> Vec<u8> {
    let data = header.data();
    let index = header.index();
    let solution_bytes = data.solution.as_bytes();

    let mut buf = Vec::with_capacity(140 + 3 + solution_bytes.len());
    buf.extend_from_slice(&data.version.to_le_bytes());
    buf.extend_from_slice(&index.parent_hash().0);
    buf.extend_from_slice(data.merkle_root());
    buf.extend_from_slice(data.block_commitments());
    buf.extend_from_slice(&(data.time() as u32).to_le_bytes());
    buf.extend_from_slice(&data.bits().to_le_bytes());
    buf.extend_from_slice(&data.nonce);
    write_compact_uint(&mut buf, solution_bytes.len() as u64);
    buf.extend_from_slice(solution_bytes);

    buf
}

/// Handle to the in-memory MMR tree, shared between the sync loop (writer)
/// and gRPC handlers (readers). `None` until the first sync completes.
pub type MmrHandle = Arc<RwLock<Option<InMemoryMmrTree>>>;

/// Create a new MMR handle (initially empty, populated on first sync).
pub fn new_mmr_handle() -> MmrHandle {
    Arc::new(RwLock::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf_index_to_mmr_pos() {
        assert_eq!(leaf_index_to_mmr_pos(0), 0);
        assert_eq!(leaf_index_to_mmr_pos(1), 1);
        assert_eq!(leaf_index_to_mmr_pos(2), 3);
        assert_eq!(leaf_index_to_mmr_pos(3), 4);
        assert_eq!(leaf_index_to_mmr_pos(4), 7);
        assert_eq!(leaf_index_to_mmr_pos(5), 8);
        assert_eq!(leaf_index_to_mmr_pos(6), 10);
        assert_eq!(leaf_index_to_mmr_pos(7), 11);
    }

    #[test]
    fn test_find_peaks() {
        assert_eq!(find_peaks(1), vec![0]);
        assert_eq!(find_peaks(3), vec![2]);
        assert_eq!(find_peaks(4), vec![2, 3]);
        assert_eq!(find_peaks(7), vec![6]);
        assert_eq!(find_peaks(8), vec![6, 7]);
        assert_eq!(find_peaks(10), vec![6, 9]);
        assert_eq!(find_peaks(11), vec![6, 9, 10]);
        assert_eq!(find_peaks(15), vec![14]);
    }

    #[test]
    fn test_work_from_nbits() {
        let work = work_from_nbits(0x1d00ffff);
        assert!(work > primitive_types::U256::zero());
        let harder = work_from_nbits(0x1c00ffff);
        assert!(harder > work);
    }

    #[test]
    fn test_serialize_v2_and_roundtrip() {
        let hash = [1u8; 32];
        let sapling = [2u8; 32];
        let orchard = [3u8; 32];
        let work = [4u8; 32];
        let bytes = serialize_v2_leaf_data(
            0x74736554, &hash, 1000, 0x1d00ffff, &sapling, &orchard, &work, 42,
        );

        // Should be parseable by zcash_history
        let node_data = V2::from_bytes(0x74736554, &bytes);
        assert!(
            node_data.is_ok(),
            "Failed to parse serialized V2 data: {:?}",
            node_data.err()
        );
    }
}
