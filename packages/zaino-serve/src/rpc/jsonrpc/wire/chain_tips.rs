//! Types associated with the `getchaintips` RPC request.

use serde::{Deserialize, Serialize};

/// Response to a `getchaintips` RPC request.
pub type GetChainTipsResponse = Vec<ChainTip>;

/// Information about a known chain tip.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChainTip {
    /// Height of the chain tip.
    pub height: u32,
    /// Block hash of the tip, in RPC display order.
    pub hash: String,
    /// Length of the branch connecting the tip to the active chain.
    pub branchlen: u32,
    /// Status of the chain tip.
    pub status: ChainTipStatus,
}

impl ChainTip {
    /// Creates a new chain tip response item.
    pub fn new(height: u32, hash: String, branchlen: u32, status: ChainTipStatus) -> Self {
        Self {
            height,
            hash,
            branchlen,
            status,
        }
    }
}

/// Status values returned by zcashd's `getchaintips`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChainTipStatus {
    /// This branch contains at least one invalid block.
    Invalid,
    /// Not all blocks for this branch are available, but the headers are valid.
    HeadersOnly,
    /// All blocks are available for this branch, but they were never fully validated.
    ValidHeaders,
    /// This branch is not part of the active chain, but is fully validated.
    ValidFork,
    /// This is the tip of the active main chain.
    Active,
    /// The validation state is unknown.
    Unknown,
}

impl ChainTip {
    /// Renders one known tip.
    pub fn from_domain(tip: zaino_primitives::types::rpc::ChainTip) -> Self {
        use zaino_primitives::types::rpc::ChainTipStatus as Domain;

        // Matched exhaustively rather than via a catch-all: `ChainTipStatus` is a
        // fixed vocabulary of the Zcash RPC interface, so a new domain variant
        // must break this and be given a spelling, not silently become
        // "unknown".
        let status = match tip.status {
            Domain::Invalid => ChainTipStatus::Invalid,
            Domain::HeadersOnly => ChainTipStatus::HeadersOnly,
            Domain::ValidHeaders => ChainTipStatus::ValidHeaders,
            Domain::ValidFork => ChainTipStatus::ValidFork,
            Domain::Active => ChainTipStatus::Active,
            Domain::Unknown => ChainTipStatus::Unknown,
        };

        Self {
            height: u32::from(tip.height),
            hash: super::display_hex(<[u8; 32]>::from(tip.hash)),
            branchlen: tip.branch_len,
            status,
        }
    }
}

/// Renders every known tip, preserving the order the index reported them in.
pub fn chain_tips_from_domain(
    tips: Vec<zaino_primitives::types::rpc::ChainTip>,
) -> GetChainTipsResponse {
    tips.into_iter().map(ChainTip::from_domain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kebab-case status spellings are the interface's vocabulary; a rename
    /// is a protocol change, so each is pinned.
    #[test]
    fn status_spellings() {
        use zaino_primitives::types::rpc::ChainTipStatus as Domain;

        for (domain, expected) in [
            (Domain::Invalid, "invalid"),
            (Domain::HeadersOnly, "headers-only"),
            (Domain::ValidHeaders, "valid-headers"),
            (Domain::ValidFork, "valid-fork"),
            (Domain::Active, "active"),
            (Domain::Unknown, "unknown"),
        ] {
            let tip = ChainTip::from_domain(zaino_primitives::types::rpc::ChainTip {
                height: zaino_primitives::types::Height::try_from(1).unwrap(),
                hash: [0; 32].into(),
                branch_len: 0,
                status: domain,
            });
            assert_eq!(
                serde_json::to_value(&tip).unwrap()["status"],
                serde_json::Value::String(expected.to_string()),
            );
        }
    }

    #[test]
    fn shape_and_display_order_hash() {
        let internal: [u8; 32] = core::array::from_fn(|i| i as u8);
        let tip = ChainTip::from_domain(zaino_primitives::types::rpc::ChainTip {
            height: zaino_primitives::types::Height::try_from(42).unwrap(),
            hash: internal.into(),
            branch_len: 3,
            status: zaino_primitives::types::rpc::ChainTipStatus::ValidFork,
        });

        assert_eq!(
            serde_json::to_value(&tip).unwrap(),
            serde_json::json!({
                "height": 42,
                "hash": super::super::display_hex(internal),
                "branchlen": 3,
                "status": "valid-fork",
            })
        );
    }
}
