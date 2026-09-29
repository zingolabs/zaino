//! `outputs` row: `txid ‖ vout → value`, big-endian (byte order = key order)

use zaino_persistence::lsm::{Key, Record};
use zaino_primitives::types::{OutPoint, Zatoshis};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRow {
    pub(crate) key: OutPoint,
    pub(crate) value: Zatoshis,
}

impl Record for OutputRow {
    type Key = OutPoint;

    const STRIDE: usize = <OutPoint as Key>::LEN + 8;

    fn key(&self) -> OutPoint {
        self.key
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key.encode());
        out.extend_from_slice(&self.value.as_u64().to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let at = <OutPoint as Key>::LEN;
        Some(Self {
            key: OutPoint::decode(bytes)?,
            value: Zatoshis::new(u64::from_be_bytes(bytes.get(at..Self::STRIDE)?.try_into().ok()?))
                .ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::TransactionId;

    use super::*;

    /// Pinned layout: txid ‖ vout BE ‖ value BE, 44 bytes; decodes back; short or out-of-supply
    /// bytes refused
    #[test]
    fn output_row_layout_is_pinned() {
        let row = OutputRow {
            key: OutPoint { txid: TransactionId::from([0xab; 32]), vout: 0x0102_0304 },
            value: Zatoshis::new(0x0506_0708).expect("in supply"),
        };
        let mut bytes = Vec::new();
        row.encode(&mut bytes);

        let golden = [
            vec![0xab; 32],
            vec![0x01, 0x02, 0x03, 0x04],
            vec![0, 0, 0, 0, 0x05, 0x06, 0x07, 0x08],
        ]
        .concat();
        assert_eq!(bytes, golden);
        assert_eq!(bytes.len(), OutputRow::STRIDE);
        assert_eq!(OutputRow::decode(&bytes), Some(row));
        assert_eq!(OutputRow::decode(&bytes[..43]), None, "short");

        let mut over_supply = bytes.clone();
        over_supply[36..].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(OutputRow::decode(&over_supply), None, "value past supply");
    }
}
