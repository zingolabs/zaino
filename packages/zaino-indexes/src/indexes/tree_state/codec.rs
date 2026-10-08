//! Frontier persistence and the zcashd legacy commitment-tree wire encoding.
//!
//! Two encodings live here, both built on the `incrementalmerkletree` /
//! `zcash_primitives` serializers rather than hand-rolled:
//!
//! - [`PersistentTreeStateValue`] — the on-disk record for a height's per-pool
//!   frontiers. Each pool is `size` (`u64`, big-endian) followed by the v1
//!   non-empty-frontier serialization (leaf + ommers) when `size > 0`. The size
//!   doubles as the empty/non-empty marker (`0` ⇒ empty tree, no frontier bytes)
//!   and is cross-checked against the frontier's own position on decode.
//! - [`legacy_tree_bytes`] — zcashd's `CommitmentTree` encoding, the exact bytes
//!   `z_gettreestate`'s `finalState` carries on the wire. It is the frontier
//!   rebuilt as a legacy [`CommitmentTree`] and serialized with
//!   [`write_commitment_tree`]. [`legacy_tree_from_bytes`] is its inverse.

use incrementalmerkletree::frontier::{CommitmentTree, Frontier};
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{DecodeError, EntryCodec, KeyOrder, PersistentRecord, RecordLayout};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zcash_primitives::merkle_tree::{
    read_commitment_tree, read_nonempty_frontier_v1, write_commitment_tree,
    write_nonempty_frontier_v1, HashSer,
};

use super::segment::DEPTH;

/// A per-pool shielded frontier (Sapling `cmu` tree) node.
type SaplingFrontier = Frontier<SaplingNode, DEPTH>;
/// A per-pool shielded frontier (Orchard / Ironwood `cmx` tree) node.
type OrchardFrontier = Frontier<MerkleHashOrchard, DEPTH>;

/// zcashd's legacy `CommitmentTree` bytes for `frontier` — the exact
/// `z_gettreestate` `finalState` encoding.
///
/// The frontier is rebuilt as a legacy [`CommitmentTree`] (its right spine as
/// `left`/`right`/`parents`) and serialized with zcashd's framing. The empty
/// frontier yields the serialized empty tree.
pub fn legacy_tree_bytes<H: HashSer + Hashable + Clone>(frontier: &Frontier<H, DEPTH>) -> Vec<u8> {
    let tree = CommitmentTree::<H, DEPTH>::from_frontier(frontier);
    let mut bytes = Vec::new();
    write_commitment_tree(&tree, &mut bytes)
        .expect("writing a commitment tree to a Vec is infallible");
    bytes
}

/// Parse zcashd legacy `CommitmentTree` bytes back into a frontier — the inverse
/// of [`legacy_tree_bytes`]. Rejects non-canonical node encodings and trees
/// deeper than [`DEPTH`].
pub fn legacy_tree_from_bytes<H: HashSer + Hashable + Clone>(
    bytes: &[u8],
) -> Result<Frontier<H, DEPTH>, DecodeError> {
    let tree = read_commitment_tree::<H, _, DEPTH>(bytes)
        .map_err(|e| DecodeError::Invalid(format!("legacy commitment tree: {e}")))?;
    Ok(tree.to_frontier())
}

/// A height's commitment-tree state: one frontier per shielded pool. A pool's
/// frontier is empty below its activation height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeStateValue {
    /// Sapling note-commitment frontier.
    pub sapling: SaplingFrontier,
    /// Orchard note-commitment frontier.
    pub orchard: OrchardFrontier,
    /// Ironwood note-commitment frontier (shares Orchard's Pallas encoding).
    pub ironwood: OrchardFrontier,
}

/// On-disk record for a [`TreeStateValue`]: Sapling, then Orchard, then Ironwood,
/// each as `size` (`u64` big-endian) and — when non-empty — the v1 non-empty
/// frontier serialization.
pub struct PersistentTreeStateValue {
    sapling: SaplingFrontier,
    orchard: OrchardFrontier,
    ironwood: OrchardFrontier,
}

impl PersistentRecord for PersistentTreeStateValue {
    type Domain = TreeStateValue;

    fn from_domain(domain: &TreeStateValue) -> Self {
        Self {
            sapling: domain.sapling.clone(),
            orchard: domain.orchard.clone(),
            ironwood: domain.ironwood.clone(),
        }
    }

    fn into_domain(self) -> Result<TreeStateValue, DecodeError> {
        Ok(TreeStateValue {
            sapling: self.sapling,
            orchard: self.orchard,
            ironwood: self.ironwood,
        })
    }
}

impl RecordLayout for PersistentTreeStateValue {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_pool(&mut bytes, &self.sapling);
        write_pool(&mut bytes, &self.orchard);
        write_pool(&mut bytes, &self.ironwood);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut rest: &[u8] = bytes;
        let sapling = read_pool::<SaplingNode>(&mut rest)?;
        let orchard = read_pool::<MerkleHashOrchard>(&mut rest)?;
        let ironwood = read_pool::<MerkleHashOrchard>(&mut rest)?;
        if !rest.is_empty() {
            return Err(DecodeError::Invalid(format!(
                "tree-state record has {} trailing bytes",
                rest.len()
            )));
        }
        Ok(Self {
            sapling,
            orchard,
            ironwood,
        })
    }
}

/// Write one pool: `size` big-endian, then the v1 non-empty frontier when the
/// tree is non-empty.
fn write_pool<H: HashSer>(bytes: &mut Vec<u8>, frontier: &Frontier<H, DEPTH>) {
    bytes.extend_from_slice(&frontier.tree_size().to_be_bytes());
    if let Some(nonempty) = frontier.value() {
        write_nonempty_frontier_v1(&mut *bytes, nonempty)
            .expect("writing a frontier to a Vec is infallible");
    }
}

/// Read one pool, advancing `rest`. Cross-checks the stored `size` against the
/// frontier's own position.
fn read_pool<H: HashSer + Hashable + Clone>(
    rest: &mut &[u8],
) -> Result<Frontier<H, DEPTH>, DecodeError> {
    let size = read_be_u64(rest)?;
    if size == 0 {
        return Ok(Frontier::empty());
    }
    let nonempty = read_nonempty_frontier_v1::<H, _>(&mut *rest)
        .map_err(|e| DecodeError::Invalid(format!("tree-state frontier: {e}")))?;
    if u64::from(nonempty.position()) + 1 != size {
        return Err(DecodeError::Invalid(format!(
            "tree-state size {size} disagrees with frontier position {}",
            u64::from(nonempty.position())
        )));
    }
    Frontier::try_from(nonempty)
        .map_err(|_| DecodeError::Invalid("tree-state frontier exceeds depth".to_owned()))
}

/// Read a big-endian `u64` from the front of `rest`, advancing it.
fn read_be_u64(rest: &mut &[u8]) -> Result<u64, DecodeError> {
    if rest.len() < 8 {
        return Err(DecodeError::Invalid(
            "tree-state record truncated before pool size".to_owned(),
        ));
    }
    let (head, tail) = rest.split_at(8);
    *rest = tail;
    Ok(u64::from_be_bytes(
        head.try_into().expect("split_at yields 8 bytes"),
    ))
}

/// The `tree_state` index codec: height → per-pool frontiers.
///
/// Hosts the on-disk format (through [`PersistentTreeStateValue`]) and the
/// format fingerprint. The sync-engine wiring (scope/composition/extraction)
/// lands in a later task; this type carries only the persistence contract.
pub struct TreeStateIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("tree_state");

impl EntryCodec for TreeStateIndex {
    type Key = BlockHeight;
    type Value = TreeStateValue;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentTreeStateValue;

    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(BlockHeight, TreeStateValue)> {
        // Exercise both the empty-tree marker and a non-empty frontier of each
        // node type, so any change to either pool's layout moves the fingerprint.
        let empty = TreeStateValue {
            sapling: Frontier::empty(),
            orchard: Frontier::empty(),
            ironwood: Frontier::empty(),
        };

        let mut sapling = Frontier::<SaplingNode, DEPTH>::empty();
        for seed in 0u8..3 {
            let mut repr = [0u8; 32];
            repr[0] = seed + 1;
            let leaf = Option::<SaplingNode>::from(SaplingNode::from_bytes(repr))
                .expect("canonical sapling sample leaf");
            assert!(sapling.append(leaf), "sample append within depth");
        }
        let mut orchard = Frontier::<MerkleHashOrchard, DEPTH>::empty();
        for seed in 0u8..3 {
            let mut repr = [0u8; 32];
            repr[0] = seed + 1;
            let leaf = Option::<MerkleHashOrchard>::from(MerkleHashOrchard::from_bytes(&repr))
                .expect("canonical orchard sample leaf");
            assert!(orchard.append(leaf), "sample append within depth");
        }
        let nonempty = TreeStateValue {
            sapling,
            orchard: orchard.clone(),
            ironwood: orchard,
        };

        vec![
            (BlockHeight::new(0), empty),
            (BlockHeight::new(1), nonempty),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{decode_value, encode_value, format_version};

    fn sapling_frontier(count: u8) -> SaplingFrontier {
        let mut frontier = Frontier::empty();
        for seed in 0..count {
            let mut repr = [0u8; 32];
            repr[0] = seed + 1;
            let leaf =
                Option::<SaplingNode>::from(SaplingNode::from_bytes(repr)).expect("canonical");
            assert!(frontier.append(leaf));
        }
        frontier
    }

    fn orchard_frontier(count: u8) -> OrchardFrontier {
        let mut frontier = Frontier::empty();
        for seed in 0..count {
            let mut repr = [0u8; 32];
            repr[0] = seed + 1;
            let leaf = Option::<MerkleHashOrchard>::from(MerkleHashOrchard::from_bytes(&repr))
                .expect("canonical");
            assert!(frontier.append(leaf));
        }
        frontier
    }

    // Step 1(d): PersistentTreeStateValue round trip, including the empty-pool
    // and non-empty-pool cases, through the codec's encode/decode helpers.
    #[test]
    fn round_trips_through_the_codec() {
        for (s, o, i) in [(0u8, 0, 0), (5, 0, 3), (1, 7, 2), (16, 16, 16)] {
            let value = TreeStateValue {
                sapling: sapling_frontier(s),
                orchard: orchard_frontier(o),
                ironwood: orchard_frontier(i),
            };
            let bytes = encode_value::<TreeStateIndex>(&value);
            let back = decode_value::<TreeStateIndex>(&bytes).expect("decode");
            assert_eq!(back, value);
        }
    }

    #[test]
    fn a_truncated_record_is_rejected() {
        let value = TreeStateValue {
            sapling: sapling_frontier(5),
            orchard: orchard_frontier(3),
            ironwood: orchard_frontier(2),
        };
        let bytes = encode_value::<TreeStateIndex>(&value);
        assert!(decode_value::<TreeStateIndex>(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn the_empty_value_encodes_as_three_zero_sizes() {
        let empty = TreeStateValue {
            sapling: Frontier::empty(),
            orchard: Frontier::empty(),
            ironwood: Frontier::empty(),
        };
        let bytes = encode_value::<TreeStateIndex>(&empty);
        assert_eq!(bytes, vec![0u8; 24], "three big-endian u64 zero sizes");
    }

    #[test]
    fn fingerprint_is_stable_and_samples_decode() {
        // The fingerprint is deterministic, and every sample round-trips through
        // the on-disk format (so the fingerprint is taken over valid bytes).
        assert_eq!(
            format_version::<TreeStateIndex>(),
            format_version::<TreeStateIndex>()
        );
        for (_, value) in TreeStateIndex::fingerprint_samples() {
            let bytes = encode_value::<TreeStateIndex>(&value);
            assert_eq!(
                decode_value::<TreeStateIndex>(&bytes).expect("decode"),
                value
            );
        }
    }
}
