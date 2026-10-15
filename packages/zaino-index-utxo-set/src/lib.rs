#![doc = include_str!("../usage.md")]

mod reader;
mod writer;

pub use reader::UtxoSetReader;
pub use writer::{fold, FoldError, UtxoSetIndexWriter};

use zaino_persistence::{IndexKind, MapTable, Schema, SequenceTable, Tables, Width};
use zaino_primitives::types::Fee;
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::value::{BalanceError, Zatoshis};
use zcash_transparent::bundle::OutPoint;

/// On-disk layout version
const FORMAT: u16 = 1;

const TABLES: Tables = Tables::new(&[FEES], &[OUTPUTS]);

/// Store layout on `network` (`zainod` opens and verifies by it)
pub const fn schema(network: NetworkType) -> Schema {
    Schema::new(IndexKind::UtxoSet, FORMAT, network, TABLES)
}

/// Buffered heap that commits a bulk run (a `FinalizedTip` block commits at once)
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256 << 20).expect("256 MiB is non-zero");

const OUTPOINT: usize = 32 + 4;
const VALUE: usize = 8;
const OUTPUTS: MapTable =
    MapTable::point(0, "outputs", OUTPOINT as u32, VALUE as u32).cache_writes().deletes();
const FEES: SequenceTable = SequenceTable::new(0, "fees", Width::Variable);

/// `txid ‖ vout` big-endian
fn encode_outpoint(outpoint: &OutPoint) -> [u8; OUTPOINT] {
    let mut out = [0u8; OUTPOINT];
    out[..32].copy_from_slice(outpoint.hash());
    out[32..].copy_from_slice(&outpoint.n().to_be_bytes());
    out
}

fn encode_value(value: Zatoshis) -> [u8; VALUE] {
    value.into_u64().to_le_bytes()
}

fn decode_value(bytes: &[u8; VALUE]) -> Result<Zatoshis, BalanceError> {
    Zatoshis::from_u64(u64::from_le_bytes(*bytes))
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
    use super::*;

    /// `outputs` row as inserted: key = `encode_outpoint`, value LE; out-of-supply refused
    #[test]
    fn an_output_row_is_its_golden_bytes_and_a_value_past_supply_is_refused() {
        let key = OutPoint::new([0xab; 32], 0x0102_0304);
        let value = Zatoshis::from_u64(0x0506_0708).expect("in supply");
        let row = [&encode_outpoint(&key)[..], &encode_value(value)].concat();

        let golden = [
            vec![0xab; 32],
            vec![0x01, 0x02, 0x03, 0x04],
            vec![0x08, 0x07, 0x06, 0x05, 0, 0, 0, 0],
        ]
        .concat();
        assert_eq!(row, golden);
        assert_eq!(decode_value(&[0x08, 0x07, 0x06, 0x05, 0, 0, 0, 0]), Ok(value));
        assert!(decode_value(&u64::MAX.to_le_bytes()).is_err(), "value past supply");
    }

    /// `fees` record = tag ‖ value LE per fee; round-trips; anything else refused
    #[test]
    fn a_fees_record_is_its_golden_bytes_and_a_malformed_one_is_refused() {
        let fees = [Fee::Coinbase, Fee::Paid(Zatoshis::from_u64(0x0102).expect("in supply"))];
        let golden = [[0, 0, 0, 0, 0, 0, 0, 0, 0], [1, 0x02, 0x01, 0, 0, 0, 0, 0, 0]].concat();
        assert_eq!(encode_fees(&fees), golden);
        assert_eq!(decode_fees(&golden), Some(fees.to_vec()));
        assert_eq!(decode_fees(&[]), Some(Vec::new()), "no transactions");

        let past_supply = [&[1][..], &u64::MAX.to_le_bytes()].concat();
        for (malformed, record) in [
            ("short", &golden[..17]),
            ("unknown tag", &[2, 0, 0, 0, 0, 0, 0, 0, 0][..]),
            ("coinbase with a value", &[0, 1, 0, 0, 0, 0, 0, 0, 0][..]),
            ("past supply", &past_supply[..]),
        ] {
            assert_eq!(decode_fees(record), None, "{malformed}");
        }
    }
}
