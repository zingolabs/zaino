//! Helpers for the `clientless` partition of the live tests go here.
//!
//! This crate also exposes test-vectors.

pub mod rpc {
    pub mod json_rpc {
        pub const VALID_P2PKH_ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
        pub const VALID_P2SH_ADDRESS: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";

        pub const VALID_SPROUT_ADDRESS: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";
        pub const VALID_PAYING_KEY: &str =
            "c8e8797f1fb5e9cf6b2d000177c5994119279a2629970a4f669aed1362a4cca5";
        pub const VALID_TRANSMISSION_KEY: &str =
            "480f78d61bdd7fc4b4edeef9f6305b29753057ab1008d42ded1a3364dac2d83c";

        pub const VALID_SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
        pub const VALID_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
        pub const VALID_DIVERSIFIED_TRANSMISSION_KEY: &str =
            "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";

        pub const VALID_UNIFIED_ADDRESS: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    }

    pub mod z_validate_address {
        use anyhow::{Context, Result};
        use serde_json::{json, Value};
        use ztest::prelude::JsonRpcClient;

        use crate::rpc::json_rpc::{
            VALID_DIVERSIFIED_TRANSMISSION_KEY, VALID_DIVERSIFIER, VALID_P2PKH_ADDRESS,
            VALID_P2SH_ADDRESS, VALID_SAPLING_ADDRESS, VALID_UNIFIED_ADDRESS,
        };

        async fn z_validate(irpc: &JsonRpcClient, addr: &str) -> Result<Value> {
            irpc.call_value("z_validateaddress", json!([addr]))
                .await
                .with_context(|| format!("z_validateaddress {addr}"))
        }

        fn assert_valid(resp: &Value, addr: &str, label: &str) {
            assert_eq!(
                resp.get("isvalid").and_then(Value::as_bool),
                Some(true),
                "{label} ({addr}) must be valid: {resp:?}"
            );
            assert_eq!(
                resp.get("address").and_then(Value::as_str),
                Some(addr),
                "{label} ({addr}) address echo: {resp:?}"
            );
        }

        pub async fn run_z_validate_suite(irpc: &JsonRpcClient) -> Result<()> {
            assert_valid(
                &z_validate(irpc, VALID_P2PKH_ADDRESS).await?,
                VALID_P2PKH_ADDRESS,
                "P2PKH",
            );
            assert_valid(
                &z_validate(irpc, VALID_P2SH_ADDRESS).await?,
                VALID_P2SH_ADDRESS,
                "P2SH",
            );
            assert_valid(
                &z_validate(irpc, VALID_UNIFIED_ADDRESS).await?,
                VALID_UNIFIED_ADDRESS,
                "Unified",
            );

            for bad in [
                "t1123456789ABCDEFGHJKLMNPQRSTUVWXY",
                "t1000000000000000000000000000000000",
            ] {
                let resp = z_validate(irpc, bad).await?;
                assert_eq!(
                    resp.get("isvalid").and_then(Value::as_bool),
                    Some(false),
                    "{bad} must be invalid: {resp:?}"
                );
            }
            Ok(())
        }

        pub async fn run_z_validate_for(irpc: &JsonRpcClient) -> Result<()> {
            run_z_validate_suite(irpc).await?;

            let s = z_validate(irpc, VALID_SAPLING_ADDRESS).await?;
            assert_eq!(
                s.get("isvalid").and_then(Value::as_bool),
                Some(true),
                "sapling must be valid: {s:?}"
            );

            // Assert the Sapling diversifier and diversified transmission key are present
            assert_eq!(
                s.get("diversifier").and_then(Value::as_str),
                Some(VALID_DIVERSIFIER),
                "sapling diversifier: {s:?}"
            );
            assert_eq!(
                s.get("diversifiedtransmissionkey").and_then(Value::as_str),
                Some(VALID_DIVERSIFIED_TRANSMISSION_KEY),
                "sapling diversifiedtransmissionkey: {s:?}"
            );
            Ok(())
        }
    }
}
