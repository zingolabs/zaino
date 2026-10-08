//! Per-pool leaf conversions: a note commitment's bytes to the Merkle hash the
//! commitment tree holds at a leaf.
//!
//! A shielded pool's note-commitment tree takes the note commitment itself as
//! its leaf: Sapling's `cmu` is a Jubjub base field element wrapped by
//! [`sapling_crypto::Node`], and Orchard's (and Ironwood's) `cmx` is a Pallas
//! base field element wrapped by [`orchard::tree::MerkleHashOrchard`]. Ironwood
//! shares Orchard's action and commitment shape, so it shares the Pallas leaf
//! encoding.
//!
//! Each conversion is the wire→domain validation step for 32 commitment bytes:
//! it returns `None` for a non-canonical field encoding rather than fabricating
//! a leaf.

use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;

/// The Sapling commitment-tree leaf for a note commitment `cmu`, or `None` if the
/// bytes are not a canonical Jubjub base field element.
pub fn sapling_leaf(cmu: [u8; 32]) -> Option<SaplingNode> {
    Option::from(SaplingNode::from_bytes(cmu))
}

/// The Orchard commitment-tree leaf for a note commitment `cmx`, or `None` if the
/// bytes are not a canonical Pallas base field element.
pub fn orchard_leaf(cmx: [u8; 32]) -> Option<MerkleHashOrchard> {
    pallas_leaf(cmx)
}

/// The Ironwood commitment-tree leaf for a note commitment `cmx`. Ironwood shares
/// Orchard's Pallas commitment encoding, so this is the same conversion.
pub fn ironwood_leaf(cmx: [u8; 32]) -> Option<MerkleHashOrchard> {
    pallas_leaf(cmx)
}

/// The shared Pallas-base leaf conversion behind [`orchard_leaf`] and
/// [`ironwood_leaf`].
fn pallas_leaf(cmx: [u8; 32]) -> Option<MerkleHashOrchard> {
    Option::from(MerkleHashOrchard::from_bytes(&cmx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sapling_round_trips_a_canonical_commitment() {
        // Small values are canonical Jubjub base elements.
        let mut bytes = [0u8; 32];
        bytes[0] = 7;
        let leaf = sapling_leaf(bytes).expect("canonical");
        assert_eq!(leaf.to_bytes(), bytes);
    }

    #[test]
    fn sapling_rejects_a_non_canonical_commitment() {
        // All-ones exceeds the field modulus, so it is non-canonical.
        assert!(sapling_leaf([0xff; 32]).is_none());
    }

    #[test]
    fn orchard_and_ironwood_round_trip_a_canonical_commitment() {
        let mut bytes = [0u8; 32];
        bytes[0] = 9;
        let orchard = orchard_leaf(bytes).expect("canonical");
        let ironwood = ironwood_leaf(bytes).expect("canonical");
        assert_eq!(orchard.to_bytes(), bytes);
        assert_eq!(ironwood.to_bytes(), bytes);
        assert_eq!(orchard, ironwood, "ironwood shares orchard's leaf encoding");
    }

    #[test]
    fn orchard_rejects_a_non_canonical_commitment() {
        assert!(orchard_leaf([0xff; 32]).is_none());
    }
}
