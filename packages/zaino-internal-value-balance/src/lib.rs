//! Transparent outpoint → value; block → per-tx [`Fee`](zaino_primitives::types::Fee) for the
//! [`FeeSink`](zaino_sync::FeeSink)
//!
//! # Data structure: one point-lookup map on the persistence port (`zaino_persistence`)
//!
//! ```text
//! outputs   OutPoint::encode() (txid 32 ‖ vout u32 BE) → value u64 BE      scope 0: never scanned
//! ```
//!
//! - insert only: spent outputs kept, never deleted (any later state re-resolves a block
//!   identically: a downstream index behind this one replays through delivery, no rewind)
//! - one block = its outputs' rows + its fees ([`fold`]); blocks above the durable tip =
//!   `zaino_persistence::Tiered`
//! - segments, merges, filters, manifest, crash safety = the engine's
//!
//! # Lookup (per transparent input, [`fold`])
//!
//! ```text
//! prevout ──▶ outputs of this block or an earlier one in the run ──hit──▶ value
//!    │ miss
//!    ▼
//! parent reader (whole run at once) ──found──▶ value
//!                                    └─────────▶ `FoldError::MissingPrevout`
//! ```

mod fold;
mod reader;
mod writer;

pub use fold::{fold, FoldError};
pub use reader::ValueBalanceReader;
pub use writer::ValueBalanceIndexWriter;

use zaino_persistence::{IndexKind, MapId, Schema, Width};
use zaino_primitives::types::{OutPoint, Zatoshis, ZatoshisOverflow};
use zcash_protocol::consensus::NetworkType;

/// On-disk layout version
const FORMAT: u16 = 1;
const OUTPUTS: MapId = MapId(0);
const VALUE: usize = 8;

/// What the store holds (`zainod verify` reads by it)
pub fn schema(network: NetworkType) -> Schema {
    let (key, value) = (Width::fixed(OutPoint::LEN as u32), Width::fixed(VALUE as u32));
    Schema::new(IndexKind::ValueBalance, FORMAT, network)
        .with_map(OUTPUTS, "outputs", key, value, 0)
}

fn encode_value(value: Zatoshis) -> [u8; VALUE] {
    value.as_u64().to_be_bytes()
}

fn decode_value(bytes: &[u8; VALUE]) -> Result<Zatoshis, ZatoshisOverflow> {
    Zatoshis::new(u64::from_be_bytes(*bytes))
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::TransactionId;

    use super::*;

    /// `outputs` row as inserted: key = `OutPoint::encode()`, value BE; out-of-supply refused
    #[test]
    fn an_output_row_is_its_golden_bytes_and_a_value_past_supply_is_refused() {
        let key = OutPoint { txid: TransactionId::from([0xab; 32]), vout: 0x0102_0304 };
        let value = Zatoshis::new(0x0506_0708).expect("in supply");
        let row = [&key.encode()[..], &encode_value(value)].concat();

        let golden = [
            vec![0xab; 32],
            vec![0x01, 0x02, 0x03, 0x04],
            vec![0, 0, 0, 0, 0x05, 0x06, 0x07, 0x08],
        ]
        .concat();
        assert_eq!(row, golden);
        assert_eq!(decode_value(&[0, 0, 0, 0, 0x05, 0x06, 0x07, 0x08]), Ok(value));
        assert!(decode_value(&u64::MAX.to_be_bytes()).is_err(), "value past supply");
    }
}
