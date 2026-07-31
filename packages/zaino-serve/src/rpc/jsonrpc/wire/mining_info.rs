//! Types associated with the `getmininginfo` RPC request.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Wire superset compatible with `zcashd` and `zebrad`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GetMiningInfoWire {
    #[serde(rename = "blocks")]
    tip_height: u64,

    #[serde(rename = "currentblocksize", default)]
    current_block_size: Option<u64>,

    #[serde(rename = "currentblocktx", default)]
    current_block_tx: Option<u64>,

    #[serde(default)]
    networksolps: Option<u64>,
    #[serde(default)]
    networkhashps: Option<u64>,

    // Present on both zcashd and zebrad
    #[serde(default)]
    chain: String,
    #[serde(default)]
    testnet: bool,

    // zcashd
    #[serde(default)]
    difficulty: Option<f64>,
    #[serde(default)]
    errors: Option<String>,
    #[serde(default)]
    errorstimestamp: Option<serde_json::Value>,
    #[serde(default)]
    genproclimit: Option<i64>,
    #[serde(default)]
    localsolps: Option<u64>,
    #[serde(default)]
    pooledtx: Option<u64>,
    #[serde(default)]
    generate: Option<bool>,

    #[serde(flatten)]
    extras: HashMap<String, serde_json::Value>,
}

/// Internal representation of `GetMiningInfoWire`.
#[derive(Debug, Clone)]
pub struct MiningInfo {
    /// Current tip height.
    pub tip_height: u64,

    /// Size of the last mined block, if present.
    pub current_block_size: Option<u64>,

    /// Transaction count in the last mined block, if present.
    pub current_block_tx: Option<u64>,

    /// Estimated network solution rate (Sol/s), if present.
    pub network_solution_rate: Option<u64>,

    /// Estimated network hash rate (H/s), if present.
    pub network_hash_rate: Option<u64>,

    /// Network name (e.g., "main", "test").
    pub chain: String,

    /// Whether the node is on testnet.
    pub testnet: bool,

    /// Current difficulty, if present.
    pub difficulty: Option<f64>,

    /// Upstream error/status message, if present.
    pub errors: Option<String>,

    /// Extra upstream fields.
    pub extras: HashMap<String, serde_json::Value>,
}

impl From<GetMiningInfoWire> for MiningInfo {
    fn from(w: GetMiningInfoWire) -> Self {
        Self {
            tip_height: w.tip_height,
            current_block_size: w.current_block_size,
            current_block_tx: w.current_block_tx,
            network_solution_rate: w.networksolps,
            network_hash_rate: w.networkhashps,
            chain: w.chain,
            testnet: w.testnet,
            difficulty: w.difficulty,
            errors: w.errors,
            extras: w.extras,
        }
    }
}

impl GetMiningInfoWire {
    /// Renders the domain type as the served JSON shape.
    ///
    /// The zcashd-only local-miner fields (`genproclimit`, `localsolps`,
    /// `generate`, `errorstimestamp`, `pooledtx`) have no counterpart in the
    /// domain type and are emitted as absent. They describe a mining daemon,
    /// which Zaino is not, so there is nothing truthful to put in them — see
    /// [`zaino_primitives::types::rpc::MiningInfo`] for the full reasoning.
    ///
    /// `extras` is likewise empty: the source layer drops fields it does not
    /// model rather than carrying opaque values through, so there is nothing to
    /// pass on.
    pub fn from_domain(info: zaino_primitives::types::rpc::MiningInfo) -> Self {
        Self {
            tip_height: info.tip_height.into(),
            current_block_size: info.current_block_size,
            current_block_tx: info.current_block_tx,
            networksolps: info.network_solution_rate,
            networkhashps: info.network_hash_rate,
            chain: info.chain,
            testnet: info.testnet,
            difficulty: info.difficulty,
            errors: info.errors,
            errorstimestamp: None,
            genproclimit: None,
            localsolps: None,
            pooledtx: None,
            generate: None,
            extras: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod from_domain_tests {
    use super::*;

    fn sample() -> zaino_primitives::types::rpc::MiningInfo {
        zaino_primitives::types::rpc::MiningInfo {
            tip_height: zaino_primitives::types::Height::try_from(1_234u32).unwrap(),
            chain: "regtest".to_string(),
            testnet: true,
            current_block_size: Some(2_000),
            current_block_tx: Some(3),
            network_solution_rate: Some(500),
            network_hash_rate: Some(600),
            difficulty: Some(1.5),
            errors: Some("something".to_string()),
        }
    }

    /// Pins zcashd's field names. `blocks`, `currentblocksize` and
    /// `currentblocktx` are all renames, so a struct-field rename here would
    /// otherwise silently change the served JSON.
    #[test]
    fn renders_zcashd_field_names() {
        let json = serde_json::to_value(GetMiningInfoWire::from_domain(sample())).unwrap();

        assert_eq!(json["blocks"], 1_234);
        assert_eq!(json["currentblocksize"], 2_000);
        assert_eq!(json["currentblocktx"], 3);
        assert_eq!(json["networksolps"], 500);
        assert_eq!(json["networkhashps"], 600);
        assert_eq!(json["chain"], "regtest");
        assert_eq!(json["testnet"], true);
        assert_eq!(json["difficulty"], 1.5);
        assert_eq!(json["errors"], "something");
    }

    /// The local-miner fields describe a component Zaino does not have, so they
    /// are emitted as JSON null rather than being given a plausible value.
    #[test]
    fn emits_the_local_miner_fields_as_absent() {
        let json = serde_json::to_value(GetMiningInfoWire::from_domain(sample())).unwrap();

        for field in [
            "genproclimit",
            "localsolps",
            "generate",
            "pooledtx",
            "errorstimestamp",
        ] {
            assert!(
                json[field].is_null(),
                "{field} describes a mining daemon and must not be invented"
            );
        }
    }

    /// The domain type's `None`s survive as nulls rather than becoming zeros —
    /// "the validator does not report this" and "the value is zero" are
    /// different answers.
    #[test]
    fn absent_domain_values_stay_absent() {
        let mut info = sample();
        info.current_block_size = None;
        info.network_solution_rate = None;
        info.difficulty = None;
        info.errors = None;

        let json = serde_json::to_value(GetMiningInfoWire::from_domain(info)).unwrap();

        assert!(json["currentblocksize"].is_null());
        assert!(json["networksolps"].is_null());
        assert!(json["difficulty"].is_null());
        assert!(json["errors"].is_null());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::jsonrpc::wire::roundtrip;
    use serde_json::json;

    fn zebrad_json() -> String {
        serde_json::to_string(&json!({
            "blocks": 1_234_567u64,
            "currentblocksize": 1_000_000u64,
            "currentblocktx": 100u64,
            "networksolps": 1234,
            "networkhashps": 5678,
            "chain": "main",
            "testnet": false,
            "errors": null,
            "somefuture": { "x": 1 }
        }))
        .unwrap()
    }

    fn zcashd_json() -> String {
        serde_json::to_string(&json!({
            "blocks": 765_432u64,
            "currentblocksize": 999_999u64,
            "currentblocktx": 99u64,
            "networksolps": 1_000_000u64,
            "networkhashps": 2_000_000u64,
            "chain": "main",
            "testnet": false,
            "difficulty": 100_000.0_f64,
            "errors": "",
            "errorstimestamp": 1700000000_i64,
            "genproclimit": 0_i64,
            "localsolps": 2500_u64,
            "pooledtx": 5_u64,
            "generate": false
        }))
        .unwrap()
    }

    #[test]
    fn deser_zebrad_then_roundtrip() {
        let json_str = zebrad_json();
        let wire: GetMiningInfoWire = serde_json::from_str(&json_str).expect("deserialize zebrad");

        assert_eq!(wire.tip_height, 1_234_567);
        assert_eq!(wire.current_block_size, Some(1_000_000));
        assert_eq!(wire.current_block_tx, Some(100));
        assert_eq!(wire.chain, "main");
        assert!(!wire.testnet);
        assert_eq!(wire.difficulty, None);
        assert_eq!(wire.errors, None);

        assert_eq!(wire.networksolps, Some(1_234));
        assert_eq!(wire.networkhashps, Some(5_678));

        assert!(wire.extras.contains_key("somefuture"));
        assert_eq!(wire.extras["somefuture"], json!({"x": 1}));

        roundtrip(&wire);
    }

    #[test]
    fn deser_zcashd_integers_then_roundtrip() {
        let json_str = zcashd_json();
        let wire: GetMiningInfoWire = serde_json::from_str(&json_str).expect("deserialize zcashd");

        assert_eq!(wire.tip_height, 765_432);
        assert_eq!(wire.current_block_size, Some(999_999));
        assert_eq!(wire.current_block_tx, Some(99));
        assert_eq!(wire.chain, "main");
        assert!(!wire.testnet);
        assert_eq!(wire.difficulty, Some(100_000.0));
        assert_eq!(wire.errors.as_deref(), Some(""));

        assert_eq!(wire.networksolps, Some(1_000_000));
        assert_eq!(wire.networkhashps, Some(2_000_000));

        assert!(wire.errorstimestamp.is_some());
        assert_eq!(wire.genproclimit, Some(0));
        assert_eq!(wire.localsolps, Some(2500));
        assert_eq!(wire.pooledtx, Some(5));
        assert_eq!(wire.generate, Some(false));

        roundtrip(&wire);
    }

    #[test]
    fn minimal_payload_defaults() {
        let blocks = r#"{ "blocks": 0 }"#;
        let wire: GetMiningInfoWire = serde_json::from_str(blocks).unwrap();

        assert_eq!(wire.tip_height, 0);
        assert_eq!(wire.current_block_size, None);
        assert_eq!(wire.current_block_tx, None);
        assert_eq!(wire.networksolps, None);
        assert_eq!(wire.networkhashps, None);

        assert_eq!(wire.chain, "");
        assert!(!wire.testnet);

        assert_eq!(wire.difficulty, None);
        assert_eq!(wire.errors, None);
        assert!(wire.extras.is_empty());

        roundtrip(&wire);
    }

    #[test]
    fn convert_to_internal_from_zebrad() {
        let wire: GetMiningInfoWire = serde_json::from_str(&zebrad_json()).unwrap();
        let mining_info: MiningInfo = wire.clone().into();

        assert_eq!(mining_info.tip_height, wire.tip_height);
        assert_eq!(mining_info.current_block_size, wire.current_block_size);
        assert_eq!(mining_info.current_block_tx, wire.current_block_tx);

        assert_eq!(mining_info.network_solution_rate, wire.networksolps);
        assert_eq!(mining_info.network_hash_rate, wire.networkhashps);

        assert_eq!(mining_info.chain, wire.chain);
        assert!(!mining_info.testnet);
        assert_eq!(mining_info.difficulty, wire.difficulty);
        assert_eq!(mining_info.errors, wire.errors);
        assert!(mining_info.extras.contains_key("somefuture"));
    }

    #[test]
    fn convert_to_internal_from_zcashd() {
        let wire: GetMiningInfoWire = serde_json::from_str(&zcashd_json()).unwrap();
        let mining_info: MiningInfo = wire.clone().into();

        assert_eq!(mining_info.tip_height, wire.tip_height);
        assert_eq!(mining_info.current_block_size, wire.current_block_size);
        assert_eq!(mining_info.current_block_tx, wire.current_block_tx);

        assert_eq!(mining_info.network_solution_rate, wire.networksolps);
        assert_eq!(mining_info.network_hash_rate, wire.networkhashps);

        assert_eq!(mining_info.chain, wire.chain);
        assert!(!mining_info.testnet);
        assert_eq!(mining_info.difficulty, wire.difficulty);
        assert_eq!(mining_info.errors, wire.errors);
    }

    #[test]
    fn invalid_numeric_type_errors() {
        let bad_str = r#"{ "blocks": 1, "networksolps": "not-a-number" }"#;
        assert!(serde_json::from_str::<GetMiningInfoWire>(bad_str).is_err());

        let bad_float = r#"{ "blocks": 1, "networkhashps": 1234.5 }"#;
        assert!(serde_json::from_str::<GetMiningInfoWire>(bad_float).is_err());

        let bad_negative = r#"{ "blocks": 1, "networkhashps": -1 }"#;
        assert!(serde_json::from_str::<GetMiningInfoWire>(bad_negative).is_err());
    }

    #[test]
    fn localsolps_roundtrip_and_reject_float() {
        let integer_payload_json = json!({
            "blocks": 3,
            "localsolps": 42_u64
        });
        let wire_int: GetMiningInfoWire = serde_json::from_value(integer_payload_json).unwrap();
        assert_eq!(wire_int.localsolps, Some(42));
        let wire_after_roundtrip: GetMiningInfoWire =
            serde_json::from_str(&serde_json::to_string(&wire_int).unwrap()).unwrap();
        assert_eq!(wire_int, wire_after_roundtrip);

        let float_payload_json_str = r#"{ "blocks": 2, "localsolps": 12.5 }"#;
        assert!(serde_json::from_str::<GetMiningInfoWire>(float_payload_json_str).is_err());
    }

    #[test]
    fn missing_network_rates_convert_to_none() {
        let json_str = r#"{ "blocks": 111, "chain": "test", "testnet": true }"#;
        let wire: GetMiningInfoWire = serde_json::from_str(json_str).unwrap();
        let mining_info: MiningInfo = wire.into();
        assert_eq!(mining_info.network_solution_rate, None);
        assert_eq!(mining_info.network_hash_rate, None);
        assert_eq!(mining_info.chain, "test");
        assert!(mining_info.testnet);
    }
}
