//! Domain -> wire conversion, owned by the adapter.
//!
//! Conversion lives here, not on the domain types, so the domain crate never
//! depends on `zaino-proto`. A local extension trait keeps the `to_wire()`
//! naming and direction while respecting the orphan rule (foreign domain type,
//! local trait).

use zaino_core::BlockId;
use zaino_proto::proto::service as proto;

pub(crate) trait ToWire {
    type Wire;
    fn to_wire(self) -> Self::Wire;
}

impl ToWire for BlockId {
    type Wire = proto::BlockId;

    fn to_wire(self) -> proto::BlockId {
        proto::BlockId {
            height: u64::from(self.height),
            hash: <[u8; 32]>::from(self.hash).to_vec(),
        }
    }
}

/// Lowercase hex, so a domain id can ride out on a wire string field without a
/// hex dependency.
pub(crate) fn to_hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
