//! Consensus-serialized payloads.
//!
//! Per ADR 0002 (`docs/driving-port/0002-raw-bytes-at-the-driving-port.md`),
//! payloads cross the port as consensus-serialized bytes: consumers own
//! their parsing, so the port never deserializes on their behalf. These
//! newtypes name the payload kind without typing its contents.
//!
//! The bytes are shared, not owned: cloning a payload is O(1) and never
//! copies the serialization, so a serving layer can hand one block or
//! transaction to many consumers without a copy per hand-out.

use core::fmt;
use std::sync::Arc;

/// A block, consensus-serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct RawBlock(Arc<[u8]>);

/// A block header, consensus-serialized.
///
/// Consensus serialization makes a block's header the prefix of the
/// block's own serialization; the conformance kit holds every
/// implementation to that.
#[derive(Clone, PartialEq, Eq)]
pub struct RawBlockHeader(Arc<[u8]>);

/// A transaction, consensus-serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct RawTransaction(Arc<[u8]>);

/// A note commitment tree frontier, serialized in the format the
/// `z_gettreestate` RPC serves (hex-decoded).
#[derive(Clone, PartialEq, Eq)]
pub struct RawTreeFrontier(Arc<[u8]>);

macro_rules! raw_payload {
    ($name:ident) => {
        impl $name {
            /// Wrap consensus-serialized bytes.
            pub fn new(bytes: Vec<u8>) -> Self {
                Self(bytes.into())
            }

            /// Borrow the serialized bytes.
            pub fn as_slice(&self) -> &[u8] {
                &self.0
            }
        }

        impl From<Vec<u8>> for $name {
            fn from(bytes: Vec<u8>) -> Self {
                Self(bytes.into())
            }
        }

        impl From<$name> for Vec<u8> {
            fn from(payload: $name) -> Self {
                // The one copying conversion: the payload is shared, so
                // taking an owned Vec must duplicate the bytes.
                payload.0.to_vec()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Payloads run to megabytes; print the size, not the bytes.
                write!(f, concat!(stringify!($name), "({} bytes)"), self.0.len())
            }
        }
    };
}

raw_payload!(RawBlock);
raw_payload!(RawBlockHeader);
raw_payload!(RawTransaction);
raw_payload!(RawTreeFrontier);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_bytes() {
        let bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let block = RawBlock::new(bytes.clone());
        assert_eq!(block.as_slice(), bytes.as_slice());
        assert_eq!(Vec::<u8>::from(block), bytes);
    }

    #[test]
    fn debug_prints_size_not_contents() {
        let tx = RawTransaction::new(vec![0u8; 1024]);
        assert_eq!(format!("{tx:?}"), "RawTransaction(1024 bytes)");
    }
}
