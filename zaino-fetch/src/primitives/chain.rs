//! Hold primitives relating to chain and network.

use crate::primitives::height::ChainHeight;
use hex::ToHex;
use std::fmt;

// /// An enum describing the kind of network, whether it's the production mainnet or a testnet.
// // Note: The order of these variants is important for correct bincode (de)serialization
// //       of history trees in the db format.
// #[derive(
//     Copy, Clone, Default, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
// )]
// pub enum NetworkKind {
//     /// The production mainnet.
//     #[default]
//     Mainnet,

//     /// A test network.
//     Testnet,

//     /// Regtest mode.
//     Regtest,
// }

/// The Consensus Branch Id, used to bind transactions and blocks to a
/// particular network upgrade.
#[derive(
    Copy, Clone, Debug, Default, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize,
)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
    ///
    /// Zebra displays consensus branch IDs in big-endian byte-order,
    /// following the convention set by zcashd.
    fn bytes_in_display_order(&self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

impl From<ConsensusBranchId> for u32 {
    fn from(branch: ConsensusBranchId) -> u32 {
        branch.0
    }
}

impl hex::ToHex for &ConsensusBranchId {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl hex::ToHex for ConsensusBranchId {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl hex::FromHex for ConsensusBranchId {
    type Error = <[u8; 4] as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let branch = <[u8; 4]>::from_hex(hex)?;
        Ok(ConsensusBranchId(u32::from_be_bytes(branch)))
    }
}

impl fmt::Display for ConsensusBranchId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

/// A hex-encoded [`ConsensusBranchId`] string.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ConsensusBranchIdHex(#[serde(with = "hex")] pub ConsensusBranchId);

/// The activation status of a [`NetworkUpgrade`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NetworkUpgradeStatus {
    /// The network upgrade is currently active.
    ///
    /// Includes all network upgrades that have previously activated,
    /// even if they are not the most recent network upgrade.
    #[serde(rename = "active")]
    Active,

    /// The network upgrade does not have an activation height.
    #[serde(rename = "disabled")]
    Disabled,

    /// The network upgrade has an activation height, but we haven't reached it yet.
    #[serde(rename = "pending")]
    Pending,
}

/// A Zcash network upgrade.
///
/// Network upgrades can change the Zcash network protocol or consensus rules in
/// incompatible ways.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NetworkUpgrade {
    /// The Zcash protocol for a Genesis block.
    ///
    /// Zcash genesis blocks use a different set of consensus rules from
    /// other BeforeOverwinter blocks, so we treat them like a separate network
    /// upgrade.
    Genesis,
    /// The Zcash protocol before the Overwinter upgrade.
    ///
    /// We avoid using `Sprout`, because the specification says that Sprout
    /// is the name of the pre-Sapling protocol, before and after Overwinter.
    BeforeOverwinter,
    /// The Zcash protocol after the Overwinter upgrade.
    Overwinter,
    /// The Zcash protocol after the Sapling upgrade.
    Sapling,
    /// The Zcash protocol after the Blossom upgrade.
    Blossom,
    /// The Zcash protocol after the Heartwood upgrade.
    Heartwood,
    /// The Zcash protocol after the Canopy upgrade.
    Canopy,
    /// The Zcash protocol after the Nu5 upgrade.
    ///
    /// Note: Network Upgrade 5 includes the Orchard Shielded Protocol, non-malleable transaction
    /// IDs, and other changes. There is no special code name for Nu5.
    #[serde(rename = "NU5")]
    Nu5,
}

impl fmt::Display for NetworkUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Same as the debug representation for now
        fmt::Debug::fmt(self, f)
    }
}

/// Information about [`NetworkUpgrade`] activation.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NetworkUpgradeInfo {
    /// Name of upgrade, string.
    pub name: NetworkUpgrade,

    /// Block height of activation, numeric.
    #[serde(rename = "activationheight")]
    pub activation_height: ChainHeight,

    /// Status of upgrade, string.
    pub status: NetworkUpgradeStatus,
}

/// The [`ConsensusBranchId`]s for the tip and the next block.
///
/// These branch IDs are different when the next block is a network upgrade activation block.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TipConsensusBranch {
    /// Branch ID used to validate the current chain tip, big-endian, hex-encoded.
    #[serde(rename = "chaintip")]
    pub chain_tip: ConsensusBranchIdHex,

    /// Branch ID used to validate the next block, big-endian, hex-encoded.
    #[serde(rename = "nextblock")]
    pub next_block: ConsensusBranchIdHex,
}
