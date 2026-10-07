//! Merkle root of the transaction tree.

use super::TransactionId;
use crate::sha256d;

/// Transaction merkle root (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MerkleRoot([u8; 32]);

impl MerkleRoot {
    /// zcashd `BlockMerkleRoot`: SHA-256d per pair, an odd level's last paired with itself
    ///
    /// - `None` = no txids, or a level pairing two equal nodes (CVE-2012-2459: same root as the
    ///   list without the repeat, so a body carrying it = a mutated body)
    pub fn of_txids(txids: &[TransactionId]) -> Option<Self> {
        let mut level: Vec<[u8; 32]> = txids.iter().map(|txid| <[u8; 32]>::from(*txid)).collect();
        while level.len() > 1 {
            if level.chunks_exact(2).any(|pair| pair[0] == pair[1]) {
                return None;
            }
            if level.len() % 2 == 1 {
                level.push(level[level.len() - 1]);
            }
            level = level.chunks(2).map(|pair| sha256d(&pair.concat())).collect();
        }
        level.first().map(|root| Self(*root))
    }
}

impl From<[u8; 32]> for MerkleRoot {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<MerkleRoot> for [u8; 32] {
    fn from(m: MerkleRoot) -> Self {
        m.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One txid = itself; a pair = SHA-256d of both; an odd level pairs its last with itself; a
    /// repeated pair (same root as without it) and an empty list have no root
    #[test]
    fn merkle_root_pairs_by_sha256d_duplicates_an_odd_last_and_refuses_a_mutated_list() {
        let [a, b, c] = [[1u8; 32], [2; 32], [3; 32]];
        let root = |txids: &[[u8; 32]]| {
            let txids: Vec<TransactionId> =
                txids.iter().copied().map(TransactionId::from).collect();
            MerkleRoot::of_txids(&txids).map(<[u8; 32]>::from)
        };
        let pair = |l: [u8; 32], r: [u8; 32]| sha256d(&[l, r].concat());
        assert_eq!(root(&[a]), Some(a));
        assert_eq!(root(&[a, b]), Some(pair(a, b)));
        assert_eq!(root(&[a, b, c]), Some(pair(pair(a, b), pair(c, c))));
        assert_eq!(root(&[a, b, c, c]), None, "CVE-2012-2459: [a b c c] roots like [a b c]");
        assert_eq!(root(&[a, a]), None);
        assert_eq!(root(&[]), None);
    }
}
