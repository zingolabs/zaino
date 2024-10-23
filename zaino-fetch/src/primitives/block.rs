//! Hold primitives relating to zcash blocks.

// use crate::primitives::error::SerializationError;
// use hex::{FromHex, ToHex};
// use std::fmt;

// /// A serialized block.
// ///
// /// Stores bytes that are guaranteed to be deserializable into a [`Block`].
// #[derive(Clone, Debug, Eq, Hash, PartialEq)]
// pub struct SerializedBlock {
//     bytes: Vec<u8>,
// }

// /// Access the serialized bytes of a [`SerializedBlock`].
// impl AsRef<[u8]> for SerializedBlock {
//     fn as_ref(&self) -> &[u8] {
//         self.bytes.as_ref()
//     }
// }

// impl From<Vec<u8>> for SerializedBlock {
//     fn from(bytes: Vec<u8>) -> Self {
//         Self { bytes }
//     }
// }

// impl serde::Serialize for SerializedBlock {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: serde::Serializer,
//     {
//         let hex_string = self.as_ref().encode_hex::<String>();
//         serializer.serialize_str(&hex_string)
//     }
// }

// impl<'de> serde::Deserialize<'de> for SerializedBlock {
//     fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
//     where
//         D: serde::Deserializer<'de>,
//     {
//         struct HexVisitor;

//         impl<'de> serde::de::Visitor<'de> for HexVisitor {
//             type Value = SerializedBlock;

//             fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
//                 formatter.write_str("a hex-encoded string")
//             }

//             fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
//             where
//                 E: serde::de::Error,
//             {
//                 let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
//                 Ok(SerializedBlock::from(bytes))
//             }
//         }

//         deserializer.deserialize_str(HexVisitor)
//     }
// }

// impl FromHex for SerializedBlock {
//     type Error = hex::FromHexError;

//     fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
//         hex::decode(hex).map(SerializedBlock::from)
//     }
// }

// /// A hash of a block, used to identify blocks and link blocks into a chain. ⛓️
// ///
// /// Technically, this is the (SHA256d) hash of a block *header*, but since the
// /// block header includes the Merkle root of the transaction Merkle tree, it
// /// binds the entire contents of the block and is used to identify entire blocks.
// ///
// /// Note: Zebra displays transaction and block hashes in big-endian byte-order,
// /// following the u256 convention set by Bitcoin and zcashd.
// ///
// /// Taken from zebra-chain for consistancy.
// #[derive(Copy, Clone, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
// pub struct BlockHash(pub [u8; 32]);

// impl BlockHash {
//     /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
//     ///
//     /// Zebra displays transaction and block hashes in big-endian byte-order,
//     /// following the u256 convention set by Bitcoin and zcashd.
//     pub fn bytes_in_display_order(&self) -> [u8; 32] {
//         let mut reversed_bytes = self.0;
//         reversed_bytes.reverse();
//         reversed_bytes
//     }

//     /// Convert bytes in big-endian byte-order into a [`block::Hash`](crate::block::Hash).
//     ///
//     /// Zebra displays transaction and block hashes in big-endian byte-order,
//     /// following the u256 convention set by Bitcoin and zcashd.
//     pub fn from_bytes_in_display_order(bytes_in_display_order: &[u8; 32]) -> BlockHash {
//         let mut internal_byte_order = *bytes_in_display_order;
//         internal_byte_order.reverse();

//         BlockHash(internal_byte_order)
//     }
// }

// impl fmt::Display for BlockHash {
//     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//         f.write_str(&self.encode_hex::<String>())
//     }
// }

// impl fmt::Debug for BlockHash {
//     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//         f.debug_tuple("block::Hash")
//             .field(&self.encode_hex::<String>())
//             .finish()
//     }
// }

// impl ToHex for &BlockHash {
//     fn encode_hex<T: FromIterator<char>>(&self) -> T {
//         self.bytes_in_display_order().encode_hex()
//     }

//     fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
//         self.bytes_in_display_order().encode_hex_upper()
//     }
// }

// impl ToHex for BlockHash {
//     fn encode_hex<T: FromIterator<char>>(&self) -> T {
//         (&self).encode_hex()
//     }

//     fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
//         (&self).encode_hex_upper()
//     }
// }

// impl FromHex for BlockHash {
//     type Error = <[u8; 32] as FromHex>::Error;

//     fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
//         let hash = <[u8; 32]>::from_hex(hex)?;

//         Ok(Self::from_bytes_in_display_order(&hash))
//     }
// }

// impl From<[u8; 32]> for BlockHash {
//     fn from(bytes: [u8; 32]) -> Self {
//         Self(bytes)
//     }
// }

// impl std::str::FromStr for BlockHash {
//     type Err = SerializationError;
//     fn from_str(s: &str) -> Result<Self, Self::Err> {
//         Ok(Self::from_hex(s)?)
//     }
// }
