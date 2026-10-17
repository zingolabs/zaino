//! `getnetworkinfo` — the backing validator's peer-to-peer network view.

use crate::types::Zatoshis;

/// The validator's network state, as reported by `getnetworkinfo`.
///
/// Facts about the node's networking, not the chain: its protocol identity, how
/// many peers it has, which networks it can reach, and its relay-fee floor.
/// Relayed whole from the validator — Zaino indexes none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    /// Validator version, as its own numeric encoding.
    pub version: u64,

    /// Network protocol user-agent string, e.g. `"/Zebra:6.4.2/"`.
    pub subversion: String,

    /// Peer-to-peer protocol version.
    pub protocol_version: u32,

    /// The services the node offers, as the hex-encoded service-flags bitfield
    /// the validator reports. A string rather than an integer because that is
    /// the wire form, and Zaino has no opinion on the bits.
    pub local_services: String,

    /// The node's clock offset from its peers, in seconds. Signed: the node may
    /// be ahead of or behind the network.
    pub time_offset: i64,

    /// Total peer connections, inbound and outbound.
    pub connections: u64,

    /// Per-network reachability, one entry per transport the node knows of.
    pub networks: Vec<NetworkEntry>,

    /// Minimum relay fee for a transaction, in zatoshis per kilobyte.
    ///
    /// The wire form is a ZEC-denominated float; the adapter converts to integer
    /// zatoshis so no rounding-prone value reaches a consumer.
    pub relay_fee: Zatoshis,

    /// The node's own advertised addresses, when it reports any.
    pub local_addresses: Vec<LocalAddress>,

    /// The validator's networking warnings, empty when there are none.
    pub warnings: String,
}

/// One transport's reachability in a [`NetworkInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    /// The network's name — `ipv4`, `ipv6`, `onion`.
    pub name: String,

    /// Whether the node will only connect to this network on explicit request.
    pub limited: bool,

    /// Whether the node considers this network reachable.
    pub reachable: bool,

    /// The proxy the node routes this network through, empty when none.
    pub proxy: String,

    /// Whether the node randomises credentials per proxy connection.
    pub proxy_randomize_credentials: bool,
}

/// One address the node advertises for itself in a [`NetworkInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAddress {
    /// The advertised address.
    pub address: String,

    /// The port the node listens on.
    pub port: u16,

    /// The node's confidence score in this address.
    pub score: i64,
}
