//! `getinfo` — general state of the backing validator.

use crate::types::{Difficulty, Height, Zatoshis};

/// General information about the backing validator.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    /// Validator version, as its own numeric encoding.
    pub version: u64,

    /// Validator build identifier.
    pub build: String,

    /// Network protocol user-agent string, e.g. `"/MagicBean:5.8.0/"`.
    pub subversion: String,

    /// Peer-to-peer protocol version.
    pub protocol_version: u32,

    /// Height of the validator's best chain.
    pub blocks: Height,

    /// Total peer connections, inbound and outbound.
    ///
    /// A fixed width, unlike the `usize` of the wire type: this crosses a
    /// network boundary, so its size must not depend on the host that parsed it.
    pub connections: u64,

    /// Current network difficulty, as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// Whether the validator considers itself to be on a test network.
    ///
    /// Prefer a chain name where one is available — this collapses testnet and
    /// regtest into a single value.
    pub testnet: bool,

    /// Proxy the validator connects through.
    ///
    /// `None` when it uses none. Zebra currently never reports one.
    pub proxy: Option<String>,

    /// Minimum transaction fee, in zatoshis per kilobyte.
    ///
    /// The wire form is a ZEC-denominated float; the adapter converts to
    /// integer zatoshis so no rounding-prone value reaches a consumer.
    pub pay_tx_fee: Zatoshis,

    /// Minimum relay fee for non-free transactions, in zatoshis per kilobyte.
    pub relay_fee: Zatoshis,

    /// The validator's last error or warning, when there is one.
    ///
    /// `None` means healthy. The wire form signals health with the sentinel
    /// string `"no errors"` rather than by omitting the field; the adapter
    /// normalises that to `None` so consumers do not have to recognise the
    /// sentinel. Matches [`MiningInfo::errors`](super::MiningInfo::errors),
    /// whose sentinel is an empty string instead.
    pub errors: Option<String>,

    /// When [`Self::errors`] was raised, in seconds since the Unix epoch.
    ///
    /// `None` whenever `errors` is `None` — the wire form carries a matching
    /// sentinel timestamp that means nothing on its own.
    pub errors_timestamp: Option<i64>,
}
