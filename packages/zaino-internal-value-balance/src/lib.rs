//! Unspent transparent outpoint → value; block → per-tx [`Fee`] for the
//! [`FeeSink`](zaino_sync::FeeSink)
//!
//! # Data structure: on the persistence port (`zaino_persistence`)
//!
//! ```text
//! outputs   OutPoint::encode() (txid 32 ‖ vout u32 BE) → value u64 BE   unspent only, scope 0
//! fees      record h = block h's fees, per tx: tag u8 (0 coinbase, 1 paid) ‖ value u64 BE
//! ```
//!
//! - `outputs` = the UTXO set: a spend removes its prevout (`deletes()`), an output created and
//!   spent in one block writes neither row
//! - fees stored: a held block's prevouts may be spent since, so its fees are read, never re-folded
//! - one block = its rows + its fees record ([`fold`]); non-final blocks = `zaino-nfs` layers
//! - segments, merges, filters, manifest, crash safety = the engine's
//!
//! # Lookup (per transparent input, [`fold`])
//!
//! ```text
//! prevout ──▶ unspent output of an earlier tx in the run ──hit──▶ value (spent: gone from the run)
//!    │ miss
//!    ▼
//! parent reader (whole run at once) ──found──▶ value
//!                                    └─────────▶ `FoldError::MissingPrevout`
//! ```

mod reader;
mod writer;

pub use reader::ValueBalanceReader;
pub use writer::{fold, FoldError, ValueBalanceIndexWriter};

use zaino_persistence::{MapTable, SequenceTable, Tables, Width};
use zaino_primitives::types::{Fee, OutPoint, Zatoshis, ZatoshisOverflow};

/// On-disk layout version
pub const FORMAT: u16 = 1;

/// What the store holds (`zainod` opens and verifies it by these)
pub const TABLES: Tables = Tables::new(&[FEES], &[OUTPUTS]);

/// Buffered heap that commits a bulk run (a `Finalized` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const VALUE: usize = 8;
const OUTPUTS: MapTable =
    MapTable::new(0, "outputs", Width::fixed(OutPoint::LEN as u32), Width::fixed(VALUE as u32), 0)
        .cache_writes()
        .deletes();
const FEES: SequenceTable = SequenceTable::new(0, "fees", Width::Variable);

fn encode_value(value: Zatoshis) -> [u8; VALUE] {
    value.as_u64().to_be_bytes()
}

fn decode_value(bytes: &[u8; VALUE]) -> Result<Zatoshis, ZatoshisOverflow> {
    Zatoshis::new(u64::from_be_bytes(*bytes))
}

/// `fees` record: per fee, a tag byte then a value
const FEE: usize = 1 + VALUE;
const COINBASE: u8 = 0;
const PAID: u8 = 1;

fn encode_fees(fees: &[Fee]) -> Vec<u8> {
    let mut record = Vec::with_capacity(fees.len() * FEE);
    for fee in fees {
        let (tag, value) = match fee {
            Fee::Coinbase => (COINBASE, Zatoshis::ZERO),
            Fee::Paid(value) => (PAID, *value),
        };
        record.push(tag);
        record.extend_from_slice(&encode_value(value));
    }
    record
}

/// `None` = not a `fees` record: a length off the stride, an unknown tag, a coinbase with a
/// value, a value past supply
fn decode_fees(record: &[u8]) -> Option<Vec<Fee>> {
    let (fees, rest) = record.as_chunks::<FEE>();
    if !rest.is_empty() {
        return None;
    }
    fees.iter()
        .map(|fee| {
            let (tag, value) = fee.split_first().expect("FEE > 0");
            let value = decode_value(value.try_into().expect("FEE = 1 + VALUE")).ok()?;
            match *tag {
                COINBASE if value == Zatoshis::ZERO => Some(Fee::Coinbase),
                PAID => Some(Fee::Paid(value)),
                _ => None,
            }
        })
        .collect()
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

    /// `fees` record = tag ‖ value BE per fee; round-trips; anything else refused
    #[test]
    fn a_fees_record_is_its_golden_bytes_and_a_malformed_one_is_refused() {
        let fees = [Fee::Coinbase, Fee::Paid(Zatoshis::new(0x0102).expect("in supply"))];
        let golden = [[0, 0, 0, 0, 0, 0, 0, 0, 0], [1, 0, 0, 0, 0, 0, 0, 0x01, 0x02]].concat();
        assert_eq!(encode_fees(&fees), golden);
        assert_eq!(decode_fees(&golden), Some(fees.to_vec()));
        assert_eq!(decode_fees(&[]), Some(Vec::new()), "no transactions");

        let past_supply = [&[1][..], &u64::MAX.to_be_bytes()].concat();
        for (malformed, record) in [
            ("short", &golden[..17]),
            ("unknown tag", &[2, 0, 0, 0, 0, 0, 0, 0, 0][..]),
            ("coinbase with a value", &[0, 0, 0, 0, 0, 0, 0, 0, 1][..]),
            ("past supply", &past_supply[..]),
        ] {
            assert_eq!(decode_fees(record), None, "{malformed}");
        }
    }
}
