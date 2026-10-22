//! Disk layouts of `outputs` (outpoint → value) and `fees` (per-tx fee records)

use zaino_primitives::types::Fee;
use zcash_protocol::value::{BalanceError, Zatoshis};
use zcash_transparent::bundle::OutPoint;

pub(super) const OUTPOINT: usize = 32 + 4;
pub(super) const VALUE: usize = 8;

/// `txid ‖ vout` big-endian
pub(super) fn encode_outpoint(outpoint: &OutPoint) -> [u8; OUTPOINT] {
    let mut out = [0u8; OUTPOINT];
    out[..32].copy_from_slice(outpoint.hash());
    out[32..].copy_from_slice(&outpoint.n().to_be_bytes());
    out
}

pub(super) fn encode_value(value: Zatoshis) -> [u8; VALUE] {
    value.into_u64().to_le_bytes()
}

pub(super) fn decode_value(bytes: &[u8; VALUE]) -> Result<Zatoshis, BalanceError> {
    Zatoshis::from_u64(u64::from_le_bytes(*bytes))
}

/// `fees` record: per fee, a tag byte then a value
const FEE: usize = 1 + VALUE;
const COINBASE: u8 = 0;
const PAID: u8 = 1;

pub(super) fn encode_fees(fees: &[Fee]) -> Vec<u8> {
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
pub(super) fn decode_fees(record: &[u8]) -> Option<Vec<Fee>> {
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

    /// - `outputs` row as inserted: key = `encode_outpoint`, value LE
    /// - out-of-supply refused
    #[test]
    fn an_output_row_is_its_golden_bytes_and_a_value_past_supply_is_refused() {
        let outpoint = OutPoint::new([0xab; 32], 0x0102_0304);
        let value = Zatoshis::from_u64(0x0506_0708).expect("in supply");
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        let row = [&encode_outpoint(&outpoint)[..], &encode_value(value)].concat();

        let golden: [(&str, String); 3] = [
            ("key: txid", "ab".repeat(32)),
            ("key: vout BE", "01020304".into()),
            ("value: zats LE", "0807060500000000".into()),
        ];
        let mut at = 0;
        for (field, expected) in golden {
            let width = expected.len() / 2;
            assert_eq!(row.get(at..at + width).map(hex), Some(expected), "{field} at byte {at}");
            at += width;
        }
        assert_eq!(hex(&row[at..]), "", "trailing bytes after byte {at}");
        assert_eq!(decode_value(&encode_value(value)), Ok(value), "decode(encode(value))");
        let past_supply = decode_value(&u64::MAX.to_le_bytes());
        assert_eq!(past_supply, Err(BalanceError::Overflow), "u64::MAX zats: past supply");
    }

    /// - `fees` record = tag ‖ value LE per fee, round-trips
    /// - anything else refused
    #[test]
    fn a_fees_record_is_its_golden_bytes_and_a_malformed_one_is_refused() {
        let fees = [Fee::Coinbase, Fee::Paid(Zatoshis::from_u64(0x0102).expect("in supply"))];
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        let record = encode_fees(&fees);

        let golden = [
            ("fee 0: tag coinbase", "00"),
            ("fee 0: zats LE", "0000000000000000"),
            ("fee 1: tag paid", "01"),
            ("fee 1: zats LE", "0201000000000000"),
        ];
        let mut at = 0;
        for (field, expected) in golden {
            let width = expected.len() / 2;
            let got = record.get(at..at + width).map(hex);
            assert_eq!(got.as_deref(), Some(expected), "{field} at byte {at}");
            at += width;
        }
        assert_eq!(hex(&record[at..]), "", "trailing bytes after byte {at}");
        assert_eq!(decode_fees(&record), Some(fees.to_vec()), "decode(encode(fees))");
        assert_eq!(decode_fees(&[]), Some(Vec::new()), "no transactions");

        let past_supply = [&[1][..], &u64::MAX.to_le_bytes()].concat();
        for (malformed, record) in [
            ("short: 17 of 18 bytes", &record[..17]),
            ("unknown tag", &[2, 0, 0, 0, 0, 0, 0, 0, 0][..]),
            ("coinbase with a value", &[0, 1, 0, 0, 0, 0, 0, 0, 0][..]),
            ("past supply", &past_supply[..]),
        ] {
            assert_eq!(decode_fees(record), None, "{malformed}: {}", hex(record));
        }
    }
}
