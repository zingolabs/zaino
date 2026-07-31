//! The `z_gettreestate` response.
//!
//! Reuses Zebra's `GetTreestateResponse`, so this module holds only the
//! conversion from the domain.

use zaino_primitives::types::{PoolTreestate, Treestate};
use zebra_rpc::client::{Commitments, GetTreestateResponse, Treestate as WireTreestate};

/// Renders one pool's treestate as the served shape.
///
/// `finalRoot` is written in **display order** — byte-reversed from the
/// domain's internal order. Note the contrast with `z_getsubtreesbyindex`,
/// whose subtree roots are *not* reversed: this asymmetry is the interface's,
/// preserved rather than tidied, because either choice produces valid-looking
/// hex and a client comparing roots across the two methods would silently see
/// them disagree.
fn pool(pool: Option<PoolTreestate>) -> WireTreestate {
    let (final_root, final_state) = match pool {
        Some(pool) => (
            pool.final_root.map(|root| {
                let mut bytes = <[u8; 32]>::from(root);
                bytes.reverse();
                bytes.to_vec()
            }),
            Some(pool.final_state),
        ),
        None => (None, None),
    };

    WireTreestate::new(Commitments::new(final_root, final_state))
}

/// Renders the domain type as the `z_gettreestate` response.
///
/// Sprout is never served: Zaino does not index it, and reporting an empty tree
/// would claim knowledge it does not have.
pub fn from_domain(trees: Treestate) -> GetTreestateResponse {
    GetTreestateResponse::new(
        zebra_chain::block::Hash(trees.block_hash.into()),
        zebra_chain::block::Height(trees.height.into()),
        trees.time,
        None,
        pool(trees.sapling),
        pool(trees.orchard),
        // The ironwood field is `Some` only from NU6.3, so pre-NU6.3 responses
        // omit it exactly as zebrad does.
        trees.ironwood.map(|tree| self::pool(Some(tree))),
    )
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
