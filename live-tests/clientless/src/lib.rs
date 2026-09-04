//! Helpers for the `clientless` partition of the live tests go here.
//!
//! This crate also exposes test-vectors.

pub mod rpc {
    pub mod json_rpc {
        // Only the Sapling vector is kept: it is the one kind carrying key
        // material, so it exercises every part of the served shape. The full
        // per-kind table (including Sprout, which Zaino reports invalid rather
        // than classifying) is a unit test in `zaino-serve`'s `wire::address`.
        pub const VALID_SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
        pub const VALID_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
        pub const VALID_DIVERSIFIED_TRANSMISSION_KEY: &str =
            "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";
    }

    pub mod z_validate_address {
        use anyhow::{Context, Result};
        use serde_json::{json, Value};
        use ztest::prelude::JsonRpcClient;

        use crate::rpc::json_rpc::{
            VALID_DIVERSIFIED_TRANSMISSION_KEY, VALID_DIVERSIFIER, VALID_SAPLING_ADDRESS,
        };

        /// Asserts the served `z_validateaddress` endpoint reaches the
        /// classifier and renders its full shape, key material included.
        ///
        /// Classification itself is not retested here — a live topology adds
        /// nothing to a pure function of (address, network). The kind is
        /// checked under both `address_type` and the legacy `type`, because
        /// serving it twice is a wire contract nothing else pins on the wire.
        pub async fn run_z_validate_for(irpc: &JsonRpcClient) -> Result<()> {
            let resp: Value = irpc
                .call_value("z_validateaddress", json!([VALID_SAPLING_ADDRESS]))
                .await
                .with_context(|| format!("z_validateaddress {VALID_SAPLING_ADDRESS}"))?;

            assert_eq!(
                resp,
                json!({
                    "isvalid": true,
                    "address": VALID_SAPLING_ADDRESS,
                    "type": "sapling",
                    "address_type": "sapling",
                    "diversifier": VALID_DIVERSIFIER,
                    "diversifiedtransmissionkey": VALID_DIVERSIFIED_TRANSMISSION_KEY,
                }),
                "z_validateaddress served shape"
            );
            Ok(())
        }
    }
}
