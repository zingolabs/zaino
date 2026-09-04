//! Types associated with the `getpeerinfo` RPC request.
//!
//! Although the current threat model assumes that `zaino` connects to a trusted validator,
//! the `getpeerinfo` RPC performs some light validation.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Response to a `getpeerinfo` RPC request.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum GetPeerInfo {
    /// The `zebrad` typed response.
    Zebrad(Vec<ZebradPeerInfo>),

    /// Unrecognized shape. Only enforced to be an array.
    Unknown(Vec<Value>),
}

impl GetPeerInfo {
    /// Renders the domain listing as the served JSON shape.
    ///
    /// Always the zebrad variant. The domain type models the two fields every
    /// validator reports, which is exactly that shape; the richer the legacy full node
    /// listing has no domain counterpart to render from. See
    /// [`zaino_primitives::types::rpc::PeerInfo`] for why.
    pub fn from_domain(peers: Vec<zaino_primitives::types::rpc::PeerInfo>) -> Self {
        Self::Zebrad(
            peers
                .into_iter()
                .map(|peer| ZebradPeerInfo {
                    addr: peer.addr,
                    inbound: peer.inbound,
                })
                .collect(),
        )
    }
}

/// Response to a `getpeerinfo` RPC request coming from `zebrad`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ZebradPeerInfo {
    /// Remote address `host:port`.
    pub addr: String,
    /// Whether the connection is inbound.
    pub inbound: bool,
}

impl<'de> Deserialize<'de> for GetPeerInfo {
    /// Deserialize a `ZebradPeerInfo` array, preserving any other array shape
    /// as `Unknown` for passthrough/logging.
    ///
    /// If the value is not an array, an error is returned.
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = Value::deserialize(de)?;

        // zebrad
        if let Ok(zebra) = serde_json::from_value::<Vec<ZebradPeerInfo>>(v.clone()) {
            return Ok(GetPeerInfo::Zebrad(zebra));
        }
        // unknown
        if v.is_array() {
            let raw: Vec<Value> = serde_json::from_value(v).map_err(serde::de::Error::custom)?;
            Ok(GetPeerInfo::Unknown(raw))
        } else {
            Err(serde::de::Error::custom("getpeerinfo: expected JSON array"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // use pretty_assertions::assert_eq;

    /// A domain listing always renders as the zebrad shape, with the legacy full node's field
    /// names and no wrapper object around the array.
    #[test]
    fn from_domain_renders_the_zebrad_shape() {
        let wire = GetPeerInfo::from_domain(vec![
            zaino_primitives::types::rpc::PeerInfo {
                addr: "127.0.0.1:8233".to_string(),
                inbound: false,
            },
            zaino_primitives::types::rpc::PeerInfo {
                addr: "example.onion:8233".to_string(),
                inbound: true,
            },
        ]);

        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            serde_json::json!([
                { "addr": "127.0.0.1:8233", "inbound": false },
                { "addr": "example.onion:8233", "inbound": true },
            ])
        );
    }

    /// No peers is an empty array, not a null or an absent field.
    #[test]
    fn from_domain_renders_no_peers_as_an_empty_array() {
        assert_eq!(
            serde_json::to_value(GetPeerInfo::from_domain(Vec::new())).unwrap(),
            serde_json::json!([])
        );
    }

    // TODO: get a real testvector
    #[test]
    fn parses_zebrad_payload() {
        let zebrad_json = r#"
        [
          { "addr": "1.2.3.4:8233", "inbound": true },
          { "addr": "5.6.7.8:8233", "inbound": false }
        ]
        "#;

        let parsed: GetPeerInfo = serde_json::from_str(zebrad_json).unwrap();
        match parsed {
            GetPeerInfo::Zebrad(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].addr, "1.2.3.4:8233");
                assert!(items[0].inbound);
                assert_eq!(items[1].addr, "5.6.7.8:8233");
                assert!(!items[1].inbound);
            }
            other => panic!("expected Zebrad variant, got: {:?}", other),
        }
    }

    /// Integrity test that ensures no Downgrade-to-Zebrad via type poisoning is possible.
    #[test]
    fn zebrad_does_not_act_as_catchall() {
        let extra_field_json = r#"
        [
            { "addr": "1.2.3.4:8233", "inbound": false, "whitelisted": "true" }
        ]
        "#;

        let parsed: GetPeerInfo = serde_json::from_str(extra_field_json).unwrap();

        match parsed {
            GetPeerInfo::Unknown(items) => {
                assert_eq!(items.len(), 1);
            }
            other => {
                panic!("expected Unknown variant, got: {:?}", other);
            }
        }
    }

    // TODO: get a real testvector
    #[test]
    fn falls_back_to_unknown_for_unrecognized_shape() {
        let unknown_json = r#"
        [
          { "foo": 1, "bar": "baz" },
          { "weird": [1,2,3] }
        ]
        "#;

        let parsed: GetPeerInfo = serde_json::from_str(unknown_json).unwrap();
        match parsed {
            GetPeerInfo::Unknown(items) => {
                assert_eq!(items.len(), 2);
                assert!(items[0].get("foo").is_some());
            }
            other => panic!("expected Unknown variant, got: {:?}", other),
        }
    }

    // TODO: get a real testvector
    #[test]
    fn fails_on_non_array() {
        let non_array_json = r#"{"foo": 1, "bar": "baz"}"#;
        let err = serde_json::from_str::<GetPeerInfo>(non_array_json).unwrap_err();
        assert_eq!(err.to_string(), "getpeerinfo: expected JSON array");
    }

    #[test]
    fn getpeerinfo_unknown_serializes_as_raw_array() {
        let val = GetPeerInfo::Unknown(vec![serde_json::json!({"foo":1})]);
        let s = serde_json::to_string(&val).unwrap();
        assert_eq!(s, r#"[{"foo":1}]"#);
    }
}
