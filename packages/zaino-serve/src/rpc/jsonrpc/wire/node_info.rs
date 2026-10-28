//! The `getinfo` response.
//!
//! Reuses Zebra's `GetInfo`, so this module holds only the conversion from the
//! domain type.

use zaino_primitives::types::rpc::NodeInfo;
use zebra_rpc::methods::GetInfo;

/// Renders the domain type as the `getinfo` response.
///
/// Two boundary conventions are restored here, both of them properties of this
/// interface rather than of the node:
///
/// - Fees are reported in ZEC. The domain type carries integer zatoshis, so the
///   lossy conversion happens here and nowhere earlier.
/// - "Healthy" is spelled with a sentinel string, not by omission. The domain
///   type normalises the sentinel to `None` so a consumer can test `is_some()`
///   without knowing each method's spelling; this puts `"no errors"` back.
pub fn from_domain(info: NodeInfo) -> GetInfo {
    GetInfo::new(
        info.version,
        info.build,
        info.subversion,
        info.protocol_version,
        info.blocks.into(),
        info.connections as usize,
        info.proxy,
        info.difficulty,
        info.testnet,
        super::zats_to_zec(info.pay_tx_fee),
        super::zats_to_zec(info.relay_fee),
        info.errors.unwrap_or_else(|| "no errors".to_string()),
        info.errors_timestamp.unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{Height, Zatoshis};

    fn sample() -> NodeInfo {
        NodeInfo {
            version: 2_000_000,
            build: "v2.0.0".to_string(),
            subversion: "/Zebra:2.0.0/".to_string(),
            protocol_version: 170_120,
            blocks: Height::try_from(2_500_000u32).unwrap(),
            connections: 8,
            difficulty: 1.5,
            testnet: false,
            proxy: None,
            pay_tx_fee: Zatoshis::new(1_000).unwrap(),
            relay_fee: Zatoshis::new(100).unwrap(),
            errors: None,
            errors_timestamp: None,
        }
    }

    /// A healthy node reports the sentinel, not an empty string and not an
    /// absent field — clients test for the literal.
    #[test]
    fn healthy_is_spelled_with_the_sentinel() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert_eq!(json["errors"], "no errors");
    }

    #[test]
    fn a_real_error_is_passed_through_unchanged() {
        let mut info = sample();
        info.errors = Some("chain is stalled".to_string());

        let json = serde_json::to_value(from_domain(info)).unwrap();

        assert_eq!(json["errors"], "chain is stalled");
    }

    /// Fees are ZEC on this interface and zatoshis in the domain. Getting the
    /// factor wrong here misreports the fee by eight orders of magnitude.
    #[test]
    fn fees_are_reported_in_zec() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert_eq!(json["paytxfee"], 0.00001);
        assert_eq!(json["relayfee"], 0.000001);
    }

    #[test]
    fn carries_the_node_identity_fields() {
        let json = serde_json::to_value(from_domain(sample())).unwrap();

        assert_eq!(json["version"], 2_000_000u64);
        assert_eq!(json["build"], "v2.0.0");
        assert_eq!(json["subversion"], "/Zebra:2.0.0/");
        assert_eq!(json["blocks"], 2_500_000u64);
        assert_eq!(json["connections"], 8);
        assert_eq!(json["testnet"], false);
    }
}
