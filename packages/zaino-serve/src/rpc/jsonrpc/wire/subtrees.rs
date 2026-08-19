//! The `z_getsubtreesbyindex` response.
//!
//! Reuses Zebra's `GetSubtreesByIndexResponse`, so this module holds the
//! conversion from the domain plus the pool-name parsing that selects which
//! pool was asked about.

use zaino_primitives::types::{rpc::SubtreeRoots, ShieldedPool};
use zebra_rpc::client::{GetSubtreesByIndexResponse, SubtreeRpcData};

/// A pool name this interface does not accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid pool name \"{0}\", must be \"sapling\" or \"orchard\"")]
pub struct UnknownPoolName(String);

/// Reads the client's pool name into the domain vocabulary.
///
/// Wire → business, so this is the external-input validation step.
///
/// Ironwood is deliberately not accepted, matching the interface as it stands:
/// `z_getsubtreesbyindex` predates the pool and no client asks for it by that
/// name. Adding it is a served-surface change, not a rewire.
pub fn pool_into_domain(pool: &str) -> Result<ShieldedPool, UnknownPoolName> {
    match pool {
        "sapling" => Ok(ShieldedPool::Sapling),
        "orchard" => Ok(ShieldedPool::Orchard),
        other => Err(UnknownPoolName(other.to_string())),
    }
}

/// Renders the domain type as the `z_getsubtreesbyindex` response.
///
/// Subtree roots are *not* byte-reversed: they are commitment-tree values, not
/// identifiers, so they are hex-encoded in their natural order — unlike the
/// block hashes and txids elsewhere on this interface.
pub fn from_domain(roots: SubtreeRoots) -> GetSubtreesByIndexResponse {
    let pool = match roots.pool {
        ShieldedPool::Sapling => "sapling",
        ShieldedPool::Orchard => "orchard",
        // Exhaustive on purpose, with no catch-all: a fourth pool must break
        // this at compile time rather than be rendered under a guessed name.
        ShieldedPool::Ironwood => "ironwood",
    };

    GetSubtreesByIndexResponse::new(
        pool.to_string(),
        zebra_chain::subtree::NoteCommitmentSubtreeIndex(roots.start_index),
        roots
            .subtrees
            .into_iter()
            .map(|subtree| SubtreeRpcData {
                root: hex::encode(<[u8; 32]>::from(subtree.root)),
                end_height: zebra_chain::block::Height(subtree.end_height.into()),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{Height, SubtreeRoot, TreeRoot};

    /// Asymmetric under reversal, so an accidental byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    #[test]
    fn pool_names_round_trip_through_the_domain() {
        for (name, pool) in [
            ("sapling", ShieldedPool::Sapling),
            ("orchard", ShieldedPool::Orchard),
        ] {
            assert_eq!(pool_into_domain(name), Ok(pool));

            let json = serde_json::to_value(from_domain(SubtreeRoots {
                pool,
                start_index: 0,
                subtrees: Vec::new(),
            }))
            .unwrap();
            assert_eq!(json["pool"], name);
        }
    }

    #[test]
    fn an_unknown_pool_name_is_rejected() {
        assert_eq!(
            pool_into_domain("plasma"),
            Err(UnknownPoolName("plasma".to_string()))
        );
        // Not a typo: see `pool_into_domain`.
        assert!(pool_into_domain("ironwood").is_err());
    }

    /// A commitment-tree root is not an identifier, so it is hex-encoded in its
    /// natural order. Reversing it here would produce valid-looking hex naming
    /// a root that does not exist.
    #[test]
    fn subtree_roots_are_not_byte_reversed() {
        let json = serde_json::to_value(from_domain(SubtreeRoots {
            pool: ShieldedPool::Orchard,
            start_index: 3,
            subtrees: vec![SubtreeRoot {
                root: TreeRoot::from(ASYMMETRIC),
                end_height: Height::try_from(2_000u32).unwrap(),
            }],
        }))
        .unwrap();

        assert_eq!(json["start_index"], 3);
        assert_eq!(json["subtrees"][0]["root"], hex::encode(ASYMMETRIC));
        assert_eq!(json["subtrees"][0]["end_height"], 2_000);
    }
}
