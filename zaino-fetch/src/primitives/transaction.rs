//! Hold primitives relating to zcash transactions.

use crate::primitives::{error::SerializationError, height::ChainHeight};
use hex::ToHex;
use serde::ser::SerializeStruct;
use std::fmt;

/// Zcash note commitment tree information.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CommitmentTreeSize {
    /// Commitment tree size.
    pub size: u64,
}

/// Information about the note commitment trees.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BlockCommitmentTreeSize {
    /// Sapling commitment tree size.
    pub sapling: CommitmentTreeSize,
    /// Orchard commitment tree size.
    pub orchard: CommitmentTreeSize,
}

/// Zingo-Indexer commitment tree structure replicating functionality in Zebra.
///
/// A wrapper that contains either an Orchard or Sapling note commitment tree.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CommitmentTreestate {
    /// Commitment tree state
    pub final_state: String,
}

/// Zingo-Indexer sapling treestate.
///
/// A treestate that is included in the [`z_gettreestate`][1] RPC response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SaplingTreestate {
    /// Sapling note commitment tree.
    pub commitments: CommitmentTreestate,
}

/// Zingo-Indexer orchard treestate.
///
/// A treestate that is included in the [`z_gettreestate`][1] RPC response.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OrchardTreestate {
    /// Sapling note commitment tree.
    pub commitments: CommitmentTreestate,
}

/// A serialized transaction.
///
/// Stores bytes that are guaranteed to be deserializable into a [`Transaction`].
///
/// Sorts in lexicographic order of the transaction's serialized data.
#[derive(Clone, Eq, PartialEq, serde::Serialize)]
pub struct SerializedTransaction {
    /// Transaction bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Display for SerializedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&hex::encode(&self.bytes))
    }
}

impl std::fmt::Debug for SerializedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let data_hex = hex::encode(&self.bytes);

        f.debug_tuple("SerializedTransaction")
            .field(&data_hex)
            .finish()
    }
}

impl AsRef<[u8]> for SerializedTransaction {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl From<Vec<u8>> for SerializedTransaction {
    fn from(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl hex::FromHex for SerializedTransaction {
    type Error = <Vec<u8> as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = <Vec<u8>>::from_hex(hex)?;

        Ok(bytes.into())
    }
}

impl<'de> serde::Deserialize<'de> for SerializedTransaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if let Some(hex_str) = v.as_str() {
            let bytes = hex::decode(hex_str).map_err(serde::de::Error::custom)?;
            Ok(SerializedTransaction { bytes })
        } else {
            Err(serde::de::Error::custom("expected a hex string"))
        }
    }
}

/// A transaction ID, which uniquely identifies mined v5 transactions,
/// and all v1-v4 transactions.
///
/// Note: Zebra displays transaction and block hashes in big-endian byte-order,
/// following the u256 convention set by Bitcoin and zcashd.
///
/// "The transaction ID of a version 4 or earlier transaction is the SHA-256d hash
/// of the transaction encoding in the pre-v5 format described above.
///
/// The transaction ID of a version 5 transaction is as defined in [ZIP-244]."
/// [Spec: Transaction Identifiers]
///
/// [ZIP-244]: https://zips.z.cash/zip-0244
/// [Spec: Transaction Identifiers]: https://zips.z.cash/protocol/protocol.pdf#txnidentifiers
///
/// Taken from zebra-chain for consistency
#[derive(
    Copy, Clone, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize, Hash,
)]
pub struct TransactionHash(pub [u8; 32]);

impl From<[u8; 32]> for TransactionHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<TransactionHash> for [u8; 32] {
    fn from(hash: TransactionHash) -> Self {
        hash.0
    }
}

impl From<&TransactionHash> for [u8; 32] {
    fn from(hash: &TransactionHash) -> Self {
        (*hash).into()
    }
}

impl TransactionHash {
    /// Return the hash bytes in big-endian byte-order suitable for printing out byte by byte.
    ///
    /// Zebra displays transaction and block hashes in big-endian byte-order,
    /// following the u256 convention set by Bitcoin and zcashd.
    pub fn bytes_in_display_order(&self) -> [u8; 32] {
        let mut reversed_bytes = self.0;
        reversed_bytes.reverse();
        reversed_bytes
    }

    /// Convert bytes in big-endian byte-order into a [`transaction::Hash`](crate::transaction::Hash).
    ///
    /// Zebra displays transaction and block hashes in big-endian byte-order,
    /// following the u256 convention set by Bitcoin and zcashd.
    pub fn from_bytes_in_display_order(bytes_in_display_order: &[u8; 32]) -> TransactionHash {
        let mut internal_byte_order = *bytes_in_display_order;
        internal_byte_order.reverse();

        TransactionHash(internal_byte_order)
    }
}

impl ToHex for &TransactionHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.bytes_in_display_order().encode_hex_upper()
    }
}

impl ToHex for TransactionHash {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl hex::FromHex for TransactionHash {
    type Error = <[u8; 32] as hex::FromHex>::Error;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let mut hash = <[u8; 32]>::from_hex(hex)?;
        hash.reverse();

        Ok(hash.into())
    }
}

impl fmt::Display for TransactionHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl fmt::Debug for TransactionHash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("transaction::Hash")
            .field(&self.encode_hex::<String>())
            .finish()
    }
}

impl std::str::FromStr for TransactionHash {
    type Err = SerializationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 32];
        if hex::decode_to_slice(s, &mut bytes[..]).is_err() {
            Err(SerializationError::Parse("hex decoding error"))
        } else {
            bytes.reverse();
            Ok(TransactionHash(bytes))
        }
    }
}

/// *** THE FOLLOWING CODE IS CURRENTLY UNUSED BY ZINGO-PROXY AND UNTESTED! ***
/// ***                           TEST BEFORE USE                           ***

/// Wrapper type that can hold Sapling or Orchard subtree roots with hex encoding.
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtreeRpcData {
    /// Merkle root of the 2^16-leaf subtree.
    pub root: String,
    /// Height of the block containing the note that completed this subtree.
    pub height: ChainHeight,
}

impl SubtreeRpcData {
    /// Returns new instance of SubtreeRpcData
    pub fn new(root: String, height: ChainHeight) -> Self {
        Self { root, height }
    }
}

impl serde::Serialize for SubtreeRpcData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("SubtreeRpcData", 2)?;
        state.serialize_field("root", &self.root)?;
        state.serialize_field("height", &self.height)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for SubtreeRpcData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Inner {
            root: String,
            height: ChainHeight,
        }

        let inner = Inner::deserialize(deserializer)?;
        Ok(SubtreeRpcData {
            root: inner.root,
            height: inner.height,
        })
    }
}

impl hex::FromHex for SubtreeRpcData {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let hex_str = std::str::from_utf8(hex.as_ref())
            .map_err(|_| hex::FromHexError::InvalidHexCharacter { c: '�', index: 0 })?;

        if hex_str.len() < 8 {
            return Err(hex::FromHexError::OddLength);
        }

        let root_end_index = hex_str.len() - 8;
        let (root_hex, height_hex) = hex_str.split_at(root_end_index);

        let root = root_hex.to_string();
        let height = u32::from_str_radix(height_hex, 16)
            .map_err(|_| hex::FromHexError::InvalidHexCharacter { c: '�', index: 0 })?;

        Ok(SubtreeRpcData {
            root,
            height: ChainHeight(height),
        })
    }
}

/// Zingo-Indexer encoding of a Bitcoin script.
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ZcashScript {
    /// # Correctness
    ///
    /// Consensus-critical serialization uses [`ZcashSerialize`].
    /// [`serde`]-based hex serialization must only be used for RPCs and testing.
    #[serde(with = "hex")]
    pub script: Vec<u8>,
}

impl ZcashScript {
    /// Create a new Bitcoin script from its raw bytes.
    /// The raw bytes must not contain the length prefix.
    pub fn new(raw_bytes: &[u8]) -> Self {
        Self {
            script: raw_bytes.to_vec(),
        }
    }

    /// Return the raw bytes of the script without the length prefix.
    ///
    /// # Correctness
    ///
    /// These raw bytes do not have a length prefix.
    /// The Zcash serialization format requires a length prefix; use `zcash_serialize`
    /// and `zcash_deserialize` to create byte data with a length prefix.
    pub fn as_raw_bytes(&self) -> &[u8] {
        &self.script
    }
}

impl core::fmt::Display for ZcashScript {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(&self.encode_hex::<String>())
    }
}

impl core::fmt::Debug for ZcashScript {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_tuple("Script")
            .field(&hex::encode(&self.script))
            .finish()
    }
}

impl hex::ToHex for &ZcashScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.as_raw_bytes().encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.as_raw_bytes().encode_hex_upper()
    }
}

impl hex::ToHex for ZcashScript {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        (&self).encode_hex_upper()
    }
}

impl hex::FromHex for ZcashScript {
    type Error = hex::FromHexError;

    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        let bytes = Vec::from_hex(hex)?;
        Ok(Self { script: bytes })
    }
}

/// A note commitment subtree index, used to identify a subtree in a shielded pool.
/// Also used to count subtrees.
///
/// *** UNTESTED - TEST BEFORE USE ***
#[derive(
    Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct NoteCommitmentSubtreeIndex(pub u16);

impl fmt::Display for NoteCommitmentSubtreeIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

impl From<u16> for NoteCommitmentSubtreeIndex {
    fn from(value: u16) -> Self {
        Self(value)
    }
}

impl TryFrom<u64> for NoteCommitmentSubtreeIndex {
    type Error = std::num::TryFromIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u16::try_from(value).map(Self)
    }
}

// If we want to automatically convert NoteCommitmentSubtreeIndex to the generic integer literal
// type, we can only implement conversion into u64. (Or u16, but not both.)
impl From<NoteCommitmentSubtreeIndex> for u64 {
    fn from(value: NoteCommitmentSubtreeIndex) -> Self {
        value.0.into()
    }
}
