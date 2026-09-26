//! The `z_gettreestate` response, in zcashd's shape, and its conversion from
//! the domain.

use zaino_primitives::types::{PoolTreestate, Treestate};
use zaino_state::jsonrpc_types::opthex;

/// The `z_gettreestate` response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct GetTreestateResponse {
    /// The hash of the block the treestate belongs to, as display-order hex.
    #[serde(with = "hex")]
    hash: zebra_chain::block::Hash,
    /// The height of the block the treestate belongs to.
    height: zebra_chain::block::Height,
    /// The block's time, in seconds since the Unix epoch.
    time: u32,
    /// The Sapling treestate.
    sapling: WireTreestate,
    /// The Orchard treestate.
    orchard: WireTreestate,
    /// The Ironwood treestate, present only from NU6.3.
    #[serde(skip_serializing_if = "Option::is_none")]
    ironwood: Option<WireTreestate>,
}

/// One pool's treestate in the `z_gettreestate` response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct WireTreestate {
    /// The pool's serialized note commitment tree and root.
    commitments: Commitments,
}

/// One pool's serialized note commitment tree and its root, each hex-encoded when present.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Commitments {
    /// The tree's root.
    #[serde(
        rename = "finalRoot",
        with = "opthex",
        skip_serializing_if = "Option::is_none"
    )]
    final_root: Option<Vec<u8>>,
    /// The serialized tree.
    #[serde(
        rename = "finalState",
        with = "opthex",
        skip_serializing_if = "Option::is_none"
    )]
    final_state: Option<Vec<u8>>,
}

/// Display order for a pool's `finalRoot`, relative to the domain's internal order.
///
/// - Sapling root = jubjub `to_bytes` (little-endian) → display reverses it
/// - Orchard/Ironwood root = pallas `to_repr` → already display order
///
/// Not a tidy-up target. Consensus encodes both roots little-endian
/// (LEBS2OSP_256, spec §7.1 and §5.4.9) — the split is `zcashd`'s display
/// convention, which prints the Sapling root as a reversed `uint256` and the
/// Pallas roots as-is. Pinned by zebra's `Root::bytes_in_display_order`.
#[derive(Clone, Copy)]
enum RootOrder {
    Reversed,
    AsIs,
}

/// Renders one pool's treestate as the served shape.
///
/// Note the further contrast with `z_getsubtreesbyindex`, whose subtree roots
/// are never reversed: a client comparing roots across the two methods would
/// otherwise silently see them disagree.
fn pool(pool: Option<PoolTreestate>, order: RootOrder) -> WireTreestate {
    let (final_root, final_state) = match pool {
        Some(pool) => (
            pool.final_root.map(|root| {
                let mut bytes = <[u8; 32]>::from(root);
                if matches!(order, RootOrder::Reversed) {
                    bytes.reverse();
                }
                bytes.to_vec()
            }),
            Some(pool.final_state),
        ),
        None => (None, None),
    };

    WireTreestate {
        commitments: Commitments {
            final_root,
            final_state,
        },
    }
}

/// Renders the domain type as the `z_gettreestate` response.
///
/// Sprout is never served: Zaino does not index it, and reporting an empty tree
/// would claim knowledge it does not have.
pub fn from_domain(trees: Treestate) -> GetTreestateResponse {
    GetTreestateResponse {
        hash: zebra_chain::block::Hash(trees.block_hash.into()),
        height: zebra_chain::block::Height(trees.height.into()),
        time: trees.time,
        sapling: pool(trees.sapling, RootOrder::Reversed),
        orchard: pool(trees.orchard, RootOrder::AsIs),
        // The ironwood field is `Some` only from NU6.3, so pre-NU6.3 responses
        // omit it exactly as zebrad does.
        ironwood: trees
            .ironwood
            .map(|tree| self::pool(Some(tree), RootOrder::AsIs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{BlockHash, Height, TreeRoot};

    /// Asymmetric under reversal, so a missing or doubled byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    fn sample() -> Treestate {
        Treestate {
            block_hash: BlockHash::from(ASYMMETRIC),
            height: Height::try_from(1_000u32).unwrap(),
            time: 1_700_000_000,
            sapling: Some(PoolTreestate {
                final_root: Some(TreeRoot::from(ASYMMETRIC)),
                final_state: vec![0xde, 0xad],
            }),
            orchard: Some(PoolTreestate {
                final_root: None,
                final_state: vec![0xbe, 0xef],
            }),
            ironwood: None,
        }
    }

    fn display_order() -> String {
        let mut bytes = ASYMMETRIC;
        bytes.reverse();
        hex::encode(bytes)
    }

    /// `finalRoot` is display-order on this method. Emitting internal order
    /// would produce valid-looking hex naming a root that does not exist.
    #[test]
    fn final_root_is_written_in_display_order() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert_eq!(json["sapling"]["commitments"]["finalRoot"], display_order());
        assert_eq!(json["hash"], display_order());
    }

    /// Orchard and Ironwood roots reach the wire in display order already, so
    /// reversing them alongside Sapling's emitted valid-looking hex naming a
    /// root no chain ever had.
    #[test]
    fn only_the_sapling_root_is_reversed() {
        let mut trees = sample();
        trees.orchard = Some(PoolTreestate {
            final_root: Some(TreeRoot::from(ASYMMETRIC)),
            final_state: vec![0xbe, 0xef],
        });
        trees.ironwood = Some(PoolTreestate {
            final_root: Some(TreeRoot::from(ASYMMETRIC)),
            final_state: vec![0xfe, 0xed],
        });

        let json = serde_json::to_value(from_domain(trees)).unwrap();
        let internal = hex::encode(ASYMMETRIC);

        assert_eq!(json["sapling"]["commitments"]["finalRoot"], display_order());
        assert_eq!(json["orchard"]["commitments"]["finalRoot"], internal);
        assert_eq!(json["ironwood"]["commitments"]["finalRoot"], internal);
    }

    /// A source that does not report a root leaves the field absent rather than
    /// zeroed — a zero root is a real value, and a client cannot tell the two
    /// apart once it is written.
    #[test]
    fn an_unreported_root_is_absent_not_zero() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        let orchard = &json["orchard"]["commitments"];
        assert!(
            orchard.get("finalRoot").is_none() || orchard["finalRoot"].is_null(),
            "an unreported root must not be rendered: {orchard}"
        );
        assert_eq!(orchard["finalState"], "beef");
    }

    /// A pool with no tree at this block is omitted, not emitted as an empty
    /// tree: `z_gettreestate` keys on absence to signal pre-activation.
    #[test]
    fn a_pool_with_no_tree_is_omitted() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert!(
            json.get("ironwood").is_none() || json["ironwood"].is_null(),
            "a pre-activation pool must be absent: {json}"
        );
        assert!(
            json.get("sprout").is_none() || json["sprout"].is_null(),
            "sprout is never served: {json}"
        );
    }

    #[test]
    fn carries_the_block_identity() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert_eq!(json["height"], 1_000);
        assert_eq!(json["time"], 1_700_000_000u32);
        assert_eq!(json["sapling"]["commitments"]["finalState"], "dead");
    }
}
