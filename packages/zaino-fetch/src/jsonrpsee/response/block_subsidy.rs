//! Types associated with the `getblocksubsidy` RPC request.

use std::convert::Infallible;

use crate::jsonrpsee::{
    connector::ResponseToError,
    response::common::amount::{Zatoshis, ZecAmount},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Struct used to represent a funding stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FundingStream {
    /// A description of the funding stream recipient.
    pub recipient: String,

    /// A URL for the specification of this funding stream.
    pub specification: String,

    /// The funding stream amount in ZEC.
    ///
    /// Amount as ZEC on the wire (string or number), normalized to zatoshis.
    #[serde(rename = "value")]
    pub value: ZecAmount,

    /// Amount as zatoshis on the wire.
    #[serde(rename = "valueZat")]
    pub value_zat: Zatoshis,

    /// The address of the funding stream recipient.
    #[serde(default)]
    pub address: Option<String>,
}

/// Struct used to represent a lockbox stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockBoxStream {
    /// A description of the funding stream recipient, or the lockbox.
    pub recipient: String,

    /// A URL for the specification of this lockbox.
    pub specification: String,

    /// The amount locked in ZEC.
    ///
    /// Amount as ZEC on the wire (string or number), normalized to zatoshis.
    #[serde(rename = "value")]
    pub value: ZecAmount,

    /// The amount locked in zatoshis.
    #[serde(rename = "valueZat")]
    pub value_zat: Zatoshis,
}

/// Response to a `getblocksubsidy` RPC request. Used for both `zcashd` and `zebrad`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlockSubsidy {
    /// The mining reward amount in ZEC.
    pub miner: ZecAmount,

    /// The founders' reward amount in ZEC.
    pub founders: ZecAmount,

    /// The total value of direct funding streams in ZEC.
    #[serde(rename = "fundingstreamstotal")]
    pub funding_streams_total: ZecAmount,

    /// The total value sent to development funding lockboxes in ZEC.
    #[serde(rename = "lockboxtotal")]
    pub lockbox_total: ZecAmount,

    /// The total value of the block subsidy in ZEC.
    #[serde(rename = "totalblocksubsidy")]
    pub total_block_subsidy: ZecAmount,

    /// An array of funding stream descriptions (present only when funding streams are active).
    #[serde(
        rename = "fundingstreams",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub funding_streams: Vec<FundingStream>,

    /// An array of development fund lockbox stream descriptions (present only when lockbox streams are active).
    #[serde(
        rename = "lockboxstreams",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub lockbox_streams: Vec<LockBoxStream>,
}

/// Response to a `getblocksubsidy` RPC request.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GetBlockSubsidy {
    /// Validated payload
    Known(BlockSubsidy),

    /// Unrecognized shape
    Unknown(Value),
}

impl ResponseToError for GetBlockSubsidy {
    type RpcError = Infallible;
}

impl<'de> Deserialize<'de> for GetBlockSubsidy {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(de)?;
        if let Ok(bs) = serde_json::from_value::<BlockSubsidy>(v.clone()) {
            Ok(GetBlockSubsidy::Known(bs))
        } else {
            Ok(GetBlockSubsidy::Unknown(v))
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::jsonrpsee::response::{
        block_subsidy::{BlockSubsidy, GetBlockSubsidy},
        common::amount::Zatoshis,
    };

    #[test]
    fn zcashd_decimals_parse_to_zats() {
        let j = serde_json::json!({
          "miner": 2.5,
          "founders": 0.0,
          "fundingstreamstotal": 0.5,
          "lockboxtotal": 0.0,
          "totalblocksubsidy": 3.0,
          "fundingstreams": [
            {"recipient":"ZCG","specification":"https://spec","value":0.5,"valueZat":50_000_000,"address":"t1abc"}
          ]
        });
        let r: GetBlockSubsidy = serde_json::from_value(j).unwrap();
        match r {
            GetBlockSubsidy::Known(x) => {
                assert_eq!(u64::from(x.miner), 250_000_000);
                assert_eq!(u64::from(x.funding_streams_total), 50_000_000);
                assert_eq!(u64::from(x.total_block_subsidy), 300_000_000);
                assert_eq!(x.funding_streams.len(), 1);
                assert_eq!(x.funding_streams[0].value_zat, Zatoshis(50_000_000));
                assert_eq!(x.funding_streams[0].address.as_deref(), Some("t1abc"));
            }
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn zebrad_strings_parse_to_zats() {
        let j = serde_json::json!({
          "fundingstreams": [],
          "lockboxstreams": [],
          "miner": "2.5",
          "founders": "0.0",
          "fundingstreamstotal": "0.5",
          "lockboxtotal": "0.0",
          "totalblocksubsidy": "3.0"
        });
        let r: GetBlockSubsidy = serde_json::from_value(j).unwrap();
        match r {
            GetBlockSubsidy::Known(x) => {
                assert_eq!(u64::from(x.miner), 250_000_000);
                assert_eq!(u64::from(x.total_block_subsidy), 300_000_000);
                assert!(x.funding_streams.is_empty());
                assert!(x.lockbox_streams.is_empty());
            }
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn lockbox_streams_parse_and_match_totals_single() {
        // Top-level amounts given in zatoshis (integers) to avoid unit ambiguity.
        let j = serde_json::json!({
          "miner": 3.0,                 // 3.0 ZEC
          "founders": 0.0,
          "fundingstreamstotal": 0.0,
          "lockboxtotal": 0.5,           // 0.5 ZEC
          "totalblocksubsidy": 3.5,     // 3.5 ZEC
          "lockboxstreams": [
            {
              "recipient":"Lockbox A",
              "specification":"https://spec",
              "value": 0.5,                    // ZEC decimal on wire means parsed to zats by ZecAmount
              "valueZat": 50_000_000           // integer zats on wire
            }
          ]
        });

        let r: GetBlockSubsidy = serde_json::from_value(j).unwrap();
        match r {
            GetBlockSubsidy::Known(x) => {
                assert_eq!(x.miner.as_zatoshis(), 300_000_000);
                assert_eq!(x.lockbox_total.as_zatoshis(), 50_000_000);
                assert_eq!(x.total_block_subsidy.as_zatoshis(), 350_000_000);

                assert!(x.funding_streams.is_empty());
                assert_eq!(x.lockbox_streams.len(), 1);

                let lb = &x.lockbox_streams[0];
                assert_eq!(lb.value.as_zatoshis(), lb.value_zat.0);
                assert_eq!(lb.recipient, "Lockbox A");
            }
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn lockbox_streams_multiple_items_sum_matches_total() {
        let j = serde_json::json!({
          "miner": 0,
          "founders": 0,
          "fundingstreamstotal": 0,
          "lockboxtotal": 1.5, // 1.5 ZEC
          "totalblocksubsidy": 1.5,
          "lockboxstreams": [
            { "recipient":"L1","specification":"s1","value": "1.0","valueZat": 100_000_000 },
            { "recipient":"L2","specification":"s2","value":  "0.5","valueZat":  50_000_000 }
          ]
        });

        let r: GetBlockSubsidy = serde_json::from_value(j).unwrap();
        match r {
            GetBlockSubsidy::Known(x) => {
                assert_eq!(u64::from(x.lockbox_total), 150_000_000);
                let sum: u64 = x.lockbox_streams.iter().map(|s| s.value_zat.0).sum();
                assert_eq!(sum, u64::from(x.lockbox_total));
            }
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn lockbox_stream_rejects_address_field() {
        // LockBoxStream has no `address` field.
        // Note that this would actually get matched to the `Unknown` variant.
        let j = serde_json::json!({
          "miner": 0, "founders": 0, "fundingstreamstotal": 0,
          "lockboxtotal": 1, "totalblocksubsidy": 1,
          "lockboxstreams": [
            { "recipient":"L","specification":"s","value":"0.00000001","valueZat":1, "address":"t1should_not_be_here" }
          ]
        });

        let err = serde_json::from_value::<BlockSubsidy>(j).unwrap_err();
        assert!(
            err.to_string().contains("unknown field") && err.to_string().contains("address"),
            "expected unknown field error, got: {err}"
        );
    }

    #[test]
    fn block_subsidy_full_roundtrip_everything() {
        use crate::jsonrpsee::response::{
            block_subsidy::{BlockSubsidy, FundingStream, GetBlockSubsidy, LockBoxStream},
            common::amount::{Zatoshis, ZecAmount},
        };

        let bs = BlockSubsidy {
            // 3.0 ZEC miner, 0 founders, 0.5 funding streams, 1.5 lockboxes = 5.0 total
            miner: ZecAmount::try_from_zec_f64(3.0).unwrap(),
            founders: ZecAmount::from_zats(0),
            funding_streams_total: ZecAmount::try_from_zec_f64(0.5).unwrap(),
            lockbox_total: ZecAmount::try_from_zec_f64(1.5).unwrap(),
            total_block_subsidy: ZecAmount::try_from_zec_f64(5.0).unwrap(),

            funding_streams: vec![FundingStream {
                recipient: "ZCG".into(),
                specification: "https://spec".into(),
                value: ZecAmount::from_zats(50_000_000), // 0.5 ZEC
                value_zat: Zatoshis(50_000_000),
                address: Some("t1abc".into()),
            }],
            lockbox_streams: vec![
                LockBoxStream {
                    recipient: "Lockbox A".into(),
                    specification: "https://boxA".into(),
                    value: ZecAmount::from_zats(100_000_000), // 1.0 ZEC
                    value_zat: Zatoshis(100_000_000),
                },
                LockBoxStream {
                    recipient: "Lockbox B".into(),
                    specification: "https://boxB".into(),
                    value: ZecAmount::from_zats(50_000_000), // 0.5 ZEC
                    value_zat: Zatoshis(50_000_000),
                },
            ],
        };

        let wrapped = GetBlockSubsidy::Known(bs.clone());

        // Serialize to JSON
        let s = serde_json::to_string(&wrapped).unwrap();

        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        // Top-level amounts are integers (zats)
        assert!(v["miner"].is_number());
        assert!(v["totalblocksubsidy"].is_number());

        // Funding stream value is a decimal number, valueZat is an integer
        let fs0 = &v["fundingstreams"][0];
        assert!(fs0["value"].is_number());
        assert!(fs0["valueZat"].is_u64());
        assert!(fs0.get("address").is_some());

        // Lockbox streams have no address
        let lb0 = &v["lockboxstreams"][0];
        assert!(lb0.get("address").is_none());

        // Deserialize back
        let back: GetBlockSubsidy = serde_json::from_str(&s).unwrap();

        // Struct-level equality must hold
        assert_eq!(back, GetBlockSubsidy::Known(bs));

        // Totals match sums
        if let GetBlockSubsidy::Known(x) = back {
            let sum_funding: u64 = x.funding_streams.iter().map(|f| f.value_zat.0).sum();
            let sum_lockbox: u64 = x.lockbox_streams.iter().map(|l| l.value_zat.0).sum();
            assert_eq!(sum_funding, u64::from(x.funding_streams_total));
            assert_eq!(sum_lockbox, u64::from(x.lockbox_total));
            assert_eq!(
                u64::from(x.miner) + u64::from(x.founders) + sum_funding + sum_lockbox,
                u64::from(x.total_block_subsidy)
            );
        }
    }
}
