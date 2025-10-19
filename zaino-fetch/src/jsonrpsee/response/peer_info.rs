//! Types associated with the `getpeerinfo` RPC request.
//!
//! Although the current threat model assumes that `zaino` connects to a trusted validator,
//! the `getpeerinfo` RPC performs some light validation.

use std::convert::Infallible;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::jsonrpsee::{
    connector::ResponseToError,
    response::common::{
        BlockHeight, Bytes, MaybeHeight, NodeId, ProtocolVersion, SecondsF64, TimeOffsetSeconds,
        UnixTime,
    },
};

/// Response to a `getpeerinfo` RPC request.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum GetPeerInfo {
    /// The `zcashd` typed response.
    Zcashd(Vec<ZcashdPeerInfo>),

    /// The `zebrad` typed response.
    Zebrad(Vec<ZebradPeerInfo>),

    /// Unrecognized shape. Only enforced to be an array.
    Unknown(Vec<Value>),
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

// TODO: Do not use primitive types
/// Response to a `getpeerinfo` RPC request coming from `zcashd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ZcashdPeerInfo {
    /// Peer index (NodeId).
    pub id: NodeId,

    /// Remote address `host:port`.
    pub addr: String,

    /// Typed representation of the hex-encoded service flags.
    pub services: ServiceFlags,

    /// Whether the peer asked us to relay transactions.
    pub relaytxes: bool,

    /// Last send time (Unix seconds).
    pub lastsend: UnixTime,

    /// Last receive time (Unix seconds).
    pub lastrecv: UnixTime,

    /// Total bytes sent.
    pub bytessent: Bytes,

    /// Total bytes received.
    pub bytesrecv: Bytes,

    /// Connection time (Unix seconds).
    pub conntime: UnixTime,

    /// Clock offset (seconds, can be negative).
    pub timeoffset: TimeOffsetSeconds,

    /// Ping time (seconds).
    pub pingtime: SecondsF64,

    /// Protocol version.
    pub version: ProtocolVersion,

    /// User agent string.
    pub subver: String,

    /// Whether the connection is inbound.
    pub inbound: bool,

    /// Starting block height advertised by the peer.
    pub startingheight: MaybeHeight,

    /// Count of processed addr messages.
    pub addr_processed: u64,

    /// Count of rate-limited addr messages.
    pub addr_rate_limited: u64,

    /// Whether the peer is whitelisted.
    pub whitelisted: bool,

    /// Local address `host:port`.
    #[serde(default)]
    pub addrlocal: Option<String>,

    /// Ping wait time in seconds. Only present if > 0.0.
    #[serde(default)]
    pub pingwait: Option<SecondsF64>,

    /// Grouped validation/sync state (present when zcashd exposes state stats).
    #[serde(flatten)]
    pub state: Option<PeerStateStats>,
}

impl<'de> Deserialize<'de> for GetPeerInfo {
    /// Deserialize either a `ZcashdPeerInfo` or a `ZebradPeerInfo` depending on the shape of the JSON.
    ///
    /// In the `Unkown` variant, the raw array is preserved for passthrough/logging.
    /// If the value is not an array, an error is returned.
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = Value::deserialize(de)?;

        // zcashd first
        if let Ok(zd) = serde_json::from_value::<Vec<ZcashdPeerInfo>>(v.clone()) {
            return Ok(GetPeerInfo::Zcashd(zd));
        }
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

impl ResponseToError for GetPeerInfo {
    type RpcError = Infallible;
}

/// Bitflags for the peer's advertised services (backed by a u64).
/// Serialized as a zero-padded 16-digit lowercase hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServiceFlags(pub u64);

impl ServiceFlags {
    /// Returns the underlying bits
    pub fn bits(self) -> u64 {
        self.0
    }

    /// Returns true if the given bit is set
    pub fn has(self, mask: u64) -> bool {
        (self.0 & mask) != 0
    }

    /// Node offers full network services (bit 0).
    pub const NODE_NETWORK: u64 = 1 << 0;

    /// Legacy Bloom filter support (bit 2).
    pub const NODE_BLOOM: u64 = 1 << 2;

    /// Returns true if the `NODE_NETWORK` bit is set
    pub fn has_node_network(self) -> bool {
        self.has(Self::NODE_NETWORK)
    }

    /// Returns true if the `NODE_BLOOM` bit is set
    pub fn has_node_bloom(self) -> bool {
        self.has(Self::NODE_BLOOM)
    }

    /// Bits not recognized by this crate.
    pub fn unknown_bits(self) -> u64 {
        let known = Self::NODE_NETWORK | Self::NODE_BLOOM;
        self.bits() & !known
    }
}

impl From<u64> for ServiceFlags {
    fn from(x: u64) -> Self {
        ServiceFlags(x)
    }
}
impl From<ServiceFlags> for u64 {
    fn from(f: ServiceFlags) -> Self {
        f.0
    }
}

impl Serialize for ServiceFlags {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&format!("{:016x}", self.0))
    }
}
impl<'de> Deserialize<'de> for ServiceFlags {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;

        // Optional `0x`
        let s = s.strip_prefix("0x").unwrap_or(&s);
        u64::from_str_radix(s, 16)
            .map(ServiceFlags)
            .map_err(|e| serde::de::Error::custom(format!("invalid services hex: {e}")))
    }
}

/// Per-peer validation/sync state. Present when state stats are set. `zcashd` only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStateStats {
    /// Misbehavior score.
    pub banscore: i64,
    /// Last header height in common.
    pub synced_headers: BlockHeight,
    /// Last block height in common.
    pub synced_blocks: BlockHeight,
    /// Block heights currently requested from this peer.
    pub inflight: Vec<BlockHeight>,
}

#[cfg(test)]
mod tests {
    use super::*;
    // use pretty_assertions::assert_eq;

    // TODO: get a real testvector
    #[test]
    fn parses_zcashd_payload() {
        let zcashd_json = r#"
        [
          {
            "id": 1,
            "addr": "127.0.0.1:8233",
            "services": "0000000000000001",
            "relaytxes": true,
            "lastsend": 1690000000,
            "lastrecv": 1690000100,
            "bytessent": 1234,
            "bytesrecv": 5678,
            "conntime": 1690000000,
            "timeoffset": 0,
            "pingtime": 0.001,
            "version": 170002,
            "subver": "/MagicBean:5.8.0/",
            "inbound": false,
            "startingheight": 2000000,
            "addr_processed": 1,
            "addr_rate_limited": 0,
            "whitelisted": false,
            "addrlocal": "192.168.1.10:8233",
            "pingwait": 0.1,
            "banscore": 0,
            "synced_headers": 1999999,
            "synced_blocks": 1999999,
            "inflight": [2000000, 2000001]
          }
        ]
        "#;

        let parsed: GetPeerInfo = serde_json::from_str(zcashd_json).unwrap();
        match parsed {
            GetPeerInfo::Zcashd(items) => {
                let p = &items[0];
                assert_eq!(p.id, NodeId(1));
                assert_eq!(p.addr, "127.0.0.1:8233");
                assert_eq!(p.version, ProtocolVersion(170002));
                assert!(!p.inbound);
                assert_eq!(p.pingwait, Some(SecondsF64(0.1)));

                let st = p.state.as_ref().expect("expected state stats");
                assert_eq!(st.synced_blocks, BlockHeight::from(1999999));
                assert_eq!(st.synced_headers, BlockHeight::from(1999999));
                assert_eq!(st.banscore, 0);
                assert_eq!(
                    st.inflight,
                    vec![BlockHeight::from(2000000), BlockHeight::from(2000001),]
                );
            }
            other => panic!("expected Zcashd, got: {:?}", other),
        }
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

    #[test]
    fn zcashd_rejects_extra_fields() {
        let j = r#"[{
        "id":1,"addr":"127.0.0.1:8233","services":"0000000000000001",
        "relaytxes":true,"lastsend":1,"lastrecv":2,"bytessent":3,"bytesrecv":4,
        "conntime":5,"timeoffset":0,"pingtime":0.1,"version":170002,"subver":"/X/","inbound":false,
        "startingheight":-1,"addr_processed":0,"addr_rate_limited":0,"whitelisted":false,
        "unexpected":"oops"
    }]"#;

        // zcashd fails due to unknown field
        let err = serde_json::from_str::<Vec<ZcashdPeerInfo>>(j).unwrap_err();
        assert!(err.to_string().contains("unknown field"));

        // Should be `Unknown`
        let parsed = serde_json::from_str::<GetPeerInfo>(j).unwrap();
        matches!(parsed, GetPeerInfo::Unknown(_));
    }

    /// Integrity test that ensures no Downgrade-to-Zebrad via type poisoning is possible.
    #[test]
    fn zebrad_does_not_act_as_catchall() {
        let invalid_zcashd = r#"
        [
            { "addr": "1.2.3.4:8233", "inbound": false, "whitelisted": "true" }
        ]
        "#;

        let parsed: GetPeerInfo = serde_json::from_str(invalid_zcashd).unwrap();

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
    fn getpeerinfo_serializes_as_raw_array() {
        let val = GetPeerInfo::Zcashd(Vec::new());
        let s = serde_json::to_string(&val).unwrap();
        assert_eq!(s, "[]");
    }

    #[test]
    fn getpeerinfo_unknown_serializes_as_raw_array() {
        let val = GetPeerInfo::Unknown(vec![serde_json::json!({"foo":1})]);
        let s = serde_json::to_string(&val).unwrap();
        assert_eq!(s, r#"[{"foo":1}]"#);
    }

    mod serviceflags {
        use crate::jsonrpsee::response::{
            common::{
                BlockHeight, Bytes, MaybeHeight, NodeId, ProtocolVersion, SecondsF64,
                TimeOffsetSeconds, UnixTime,
            },
            peer_info::{ServiceFlags, ZcashdPeerInfo},
        };

        #[test]
        fn serviceflags_roundtrip() {
            let f = ServiceFlags(0x0000_0000_0000_0001);
            let s = serde_json::to_string(&f).unwrap();
            assert_eq!(s, r#""0000000000000001""#); // zero-padded, lowercase
            let back: ServiceFlags = serde_json::from_str(&s).unwrap();
            assert_eq!(back.bits(), 1);
            assert!(back.has(1));
        }

        #[test]
        fn zcashd_peerinfo_deser_with_typed_services() {
            let j = r#"[{
            "id":1,
            "addr":"127.0.0.1:8233",
            "services":"0000000000000003",
            "relaytxes":true,
            "lastsend":1,"lastrecv":2,"bytessent":3,"bytesrecv":4,
            "conntime":5,"timeoffset":0,"pingtime":0.001,
            "version":170002,"subver":"/MagicBean:5.8.0/","inbound":false,
            "startingheight":2000000,"addr_processed":7,"addr_rate_limited":8,"whitelisted":false
        }]"#;

            let v: Vec<ZcashdPeerInfo> = serde_json::from_str(j).unwrap();
            assert_eq!(v[0].services.bits(), 3);
            assert!(v[0].services.has(1));
            assert!(v[0].services.has(2));
        }

        #[test]
        fn zcashd_peerinfo_serializes_back_to_hex() {
            let pi = ZcashdPeerInfo {
                id: NodeId(1),
                addr: "127.0.0.1:8233".into(),
                services: ServiceFlags(0x0A0B_0C0D_0E0F),
                relaytxes: true,
                lastsend: UnixTime(1),
                lastrecv: UnixTime(2),
                bytessent: Bytes(3),
                bytesrecv: Bytes(4),
                conntime: UnixTime(5),
                timeoffset: TimeOffsetSeconds(0),
                pingtime: SecondsF64(0.1),
                version: ProtocolVersion(170002),
                subver: "/X/".into(),
                inbound: false,
                startingheight: MaybeHeight(Some(BlockHeight::from(42))),
                addr_processed: 0,
                addr_rate_limited: 0,
                whitelisted: false,
                addrlocal: None,
                pingwait: None,
                state: None,
            };

            let v = serde_json::to_value(&pi).unwrap();
            let services_str = v["services"].as_str().unwrap();
            let expected = format!("{:016x}", u64::from(pi.services));
            assert_eq!(services_str, expected); // "00000a0b0c0d0e0f"
        }
    }
}
