//! Types associated with the `getstandardfee` RPC request.

use serde::{Deserialize, Serialize};

/// The `getstandardfee` response, in zebrad's shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GetStandardFee {
    /// Recommended fee per logical action, in zatoshis.
    pub standard_fee: u64,

    /// Estimator version identifier.
    pub version: u32,
}

impl GetStandardFee {
    /// Renders the domain type as the served JSON shape.
    pub fn from_domain(fee: zaino_primitives::types::rpc::StandardFee) -> Self {
        Self {
            standard_fee: fee.fee_per_action.as_u64(),
            version: fee.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::jsonrpc::wire::roundtrip;
    use serde_json::json;

    /// Pins zebrad's field names: `standard_fee` is a plain zatoshi integer,
    /// never a ZEC decimal.
    #[test]
    fn renders_zebrad_field_names() {
        let fee = zaino_primitives::types::rpc::StandardFee {
            fee_per_action: zaino_primitives::types::Zatoshis::new(1_000).expect("in range"),
            version: 0,
        };

        let wire = GetStandardFee::from_domain(fee);
        assert_eq!(
            serde_json::to_value(&wire).expect("serialises"),
            json!({ "standard_fee": 1_000, "version": 0 })
        );
        roundtrip(&wire);
    }
}
