//! Metadata objects

use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use corez::io::{self, Read, Write};

use crate::{
    read_fixed_le, read_u64_le, version, write_fixed_le, write_u64_le, FixedEncodedLen,
    ZainoVersionedSerde,
};

use super::legacy::{Outpoint, ScriptType, TxOutCompact};

/// Returns `true` if `out` should be excluded from the transparent UTXO set.
///
/// Mirrors zcashd's `IsUnspendable()` for the purposes of `gettxoutsetinfo`:
/// only outputs whose script parses as P2PKH or P2SH are counted as part of
/// the UTXO set. Everything else (OP_RETURN coinbase commitments, oversized
/// or otherwise non-standard scripts) is treated as unspendable and excluded
/// from `transactions`, `transaction_outputs`, `bytes_serialized`,
/// `hash_serialized` and `total_zatoshis`.
pub fn is_unspendable_tx_out(out: &TxOutCompact) -> bool {
    !matches!(
        out.script_type_enum(),
        Some(ScriptType::P2PKH) | Some(ScriptType::P2SH),
    )
}

/// Domain separator for the Zaino transparent UTXO set commitment.
///
/// Prepended to every per-UTXO byte string before BLAKE2b-256 so the
/// commitment cannot collide with any other Zaino hash domain.
const ZAINO_TXOUTSET_DOMAIN_TAG: &[u8; 16] = b"ZcashTxOutSet___";

/// Canonical encoded length of a single UTXO entry inside the Zaino
/// transparent UTXO set commitment.
///
/// Layout (little-endian / raw): `prev_txid [u8;32] || output_index u32
/// || value u64 || script_hash [u8;20] || script_type u8` = 65 bytes.
pub const ZAINO_TXOUTSET_ENTRY_LEN: u64 = 32 + 4 + 8 + 20 + 1;

/// Computes the per-UTXO digest used by [`FinalisedTxOutSetInfoAccumulator::hash_serialized`].
///
/// `digest = BLAKE2b-256(ZAINO_TXOUTSET_DOMAIN_TAG || canonical_entry(outpoint, out))`.
///
/// The 16-byte domain tag separates this hash from any other BLAKE2b-256
/// used in Zaino. Caller XORs the returned digest into the running
/// accumulator on add and again on remove (XOR is self-inverse).
pub fn tx_out_set_entry_digest(outpoint: &Outpoint, out: &TxOutCompact) -> [u8; 32] {
    let mut hasher =
        Blake2bVar::new(32).expect("BLAKE2b-256 hasher initialises with digest_size=32");
    hasher.update(ZAINO_TXOUTSET_DOMAIN_TAG);
    hasher.update(outpoint.prev_txid());
    hasher.update(&outpoint.prev_index().to_le_bytes());
    hasher.update(&out.value().to_le_bytes());
    hasher.update(out.script_hash());
    hasher.update(&[out.script_type()]);
    let mut output = [0u8; 32];
    hasher
        .finalize_variable(&mut output)
        .expect("BLAKE2b-256 finalises with matching digest_size");
    output
}

/// Holds information about the mempool state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MempoolInfo {
    /// Current tx count
    pub size: u64,
    /// Sum of all tx sizes
    pub bytes: u64,
    /// Total memory usage for the mempool
    pub usage: u64,
}

impl ZainoVersionedSerde for MempoolInfo {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut w = w;
        write_u64_le(&mut w, self.size)?;
        write_u64_le(&mut w, self.bytes)?;
        write_u64_le(&mut w, self.usage)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut r = r;
        let size = read_u64_le(&mut r)?;
        let bytes = read_u64_le(&mut r)?;
        let usage = read_u64_le(&mut r)?;
        Ok(MempoolInfo { size, bytes, usage })
    }
}

/// 24 byte body.
impl FixedEncodedLen for MempoolInfo {
    /// 8 byte size + 8 byte bytes + 8 byte usage
    const ENCODED_LEN: usize = 8 + 8 + 8;
}

impl From<zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse> for MempoolInfo {
    fn from(resp: zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse) -> Self {
        MempoolInfo {
            size: resp.size,
            bytes: resp.bytes,
            usage: resp.usage,
        }
    }
}

impl From<MempoolInfo> for zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse {
    fn from(info: MempoolInfo) -> Self {
        zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }
}

/// Holds finalised-state UTXO set accumulator data for `gettxoutsetinfo`.
///
/// This is not the full RPC response. It only contains values that the
/// finalised-state database can maintain cheaply and exactly.
///
/// `hash_serialized` is Zaino's transparent-UTXO-set multiset commitment:
/// the XOR over all currently-unspent transparent outputs of
/// [`tx_out_set_entry_digest`]. It is not byte-equal to zcashd's value
/// and is not expected to be.
///
/// `bytes_serialized` is the total canonical byte-length of the UTXO set
/// in Zaino's representation, i.e. `transaction_outputs *
/// ZAINO_TXOUTSET_ENTRY_LEN`. Stored explicitly so the wire mapping is a
/// trivial field-copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinalisedTxOutSetInfoAccumulator {
    /// Number of transactions with at least one currently unspent transparent output.
    pub transactions: u64,

    /// Number of currently unspent transparent outputs.
    pub transaction_outputs: u64,

    /// Total canonical byte-length of the UTXO set under Zaino's encoding.
    pub bytes_serialized: u64,

    /// XOR-of-BLAKE2b-256 multiset commitment over [`tx_out_set_entry_digest`]
    /// applied to every currently-unspent transparent output.
    pub hash_serialized: [u8; 32],

    /// Sum of `value` (zatoshis) over every currently-unspent transparent output.
    pub total_zatoshis: u64,
}

impl FinalisedTxOutSetInfoAccumulator {
    /// Creates a new finalised txout-set accumulator.
    pub const fn new(
        transactions: u64,
        transaction_outputs: u64,
        bytes_serialized: u64,
        hash_serialized: [u8; 32],
        total_zatoshis: u64,
    ) -> Self {
        Self {
            transactions,
            transaction_outputs,
            bytes_serialized,
            hash_serialized,
            total_zatoshis,
        }
    }

    /// Returns an empty finalised txout-set accumulator.
    pub const fn empty() -> Self {
        Self {
            transactions: 0,
            transaction_outputs: 0,
            bytes_serialized: 0,
            hash_serialized: [0u8; 32],
            total_zatoshis: 0,
        }
    }

    /// Applies a single UTXO entering the set to all per-output fields.
    ///
    /// Mutates `transaction_outputs`, `bytes_serialized`, `total_zatoshis` and `hash_serialized`.
    /// Caller is responsible for `transactions` bookkeeping (the 0↔>0 unspent-output transition),
    /// because that requires context across multiple outputs of the same transaction.
    pub fn apply_added_output(
        &mut self,
        outpoint: &Outpoint,
        out: &TxOutCompact,
    ) -> Result<(), AccumulatorDeltaError> {
        let digest = tx_out_set_entry_digest(outpoint, out);
        for (dst, src) in self.hash_serialized.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
        self.transaction_outputs = self
            .transaction_outputs
            .checked_add(1)
            .ok_or(AccumulatorDeltaError::Overflow("transaction_outputs"))?;
        self.bytes_serialized = self
            .bytes_serialized
            .checked_add(ZAINO_TXOUTSET_ENTRY_LEN)
            .ok_or(AccumulatorDeltaError::Overflow("bytes_serialized"))?;
        self.total_zatoshis = self
            .total_zatoshis
            .checked_add(out.value())
            .ok_or(AccumulatorDeltaError::Overflow("total_zatoshis"))?;
        Ok(())
    }

    /// Applies a single UTXO leaving the set to all per-output fields. Inverse of
    /// [`Self::apply_added_output`].
    pub fn apply_removed_output(
        &mut self,
        outpoint: &Outpoint,
        out: &TxOutCompact,
    ) -> Result<(), AccumulatorDeltaError> {
        let digest = tx_out_set_entry_digest(outpoint, out);
        for (dst, src) in self.hash_serialized.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
        self.transaction_outputs = self
            .transaction_outputs
            .checked_sub(1)
            .ok_or(AccumulatorDeltaError::Underflow("transaction_outputs"))?;
        self.bytes_serialized = self
            .bytes_serialized
            .checked_sub(ZAINO_TXOUTSET_ENTRY_LEN)
            .ok_or(AccumulatorDeltaError::Underflow("bytes_serialized"))?;
        self.total_zatoshis = self
            .total_zatoshis
            .checked_sub(out.value())
            .ok_or(AccumulatorDeltaError::Underflow("total_zatoshis"))?;
        Ok(())
    }
}

/// Failure modes for accumulator delta operations.
///
/// Carries the field name so callers can produce specific error context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccumulatorDeltaError {
    /// A counter or running sum overflowed `u64`.
    #[error("txout-set accumulator {0} overflow")]
    Overflow(&'static str),
    /// A counter or running sum underflowed `u64`.
    #[error("txout-set accumulator {0} underflow")]
    Underflow(&'static str),
}

impl ZainoVersionedSerde for FinalisedTxOutSetInfoAccumulator {
    const VERSION: u8 = version::V1;

    fn encode_latest<Writer: Write>(&self, writer: &mut Writer) -> io::Result<()> {
        Self::encode_v1(self, writer)
    }

    fn decode_latest<Reader: Read>(reader: &mut Reader) -> io::Result<Self> {
        Self::decode_v1(reader)
    }

    fn encode_v1<Writer: Write>(&self, writer: &mut Writer) -> io::Result<()> {
        write_u64_le(&mut *writer, self.transactions)?;
        write_u64_le(&mut *writer, self.transaction_outputs)?;
        write_u64_le(&mut *writer, self.bytes_serialized)?;
        write_fixed_le::<32, _>(&mut *writer, &self.hash_serialized)?;
        write_u64_le(&mut *writer, self.total_zatoshis)
    }

    fn decode_v1<Reader: Read>(reader: &mut Reader) -> io::Result<Self> {
        let transactions = read_u64_le(&mut *reader)?;
        let transaction_outputs = read_u64_le(&mut *reader)?;
        let bytes_serialized = read_u64_le(&mut *reader)?;
        let hash_serialized = read_fixed_le::<32, _>(&mut *reader)?;
        let total_zatoshis = read_u64_le(&mut *reader)?;

        Ok(Self {
            transactions,
            transaction_outputs,
            bytes_serialized,
            hash_serialized,
            total_zatoshis,
        })
    }
}

impl FixedEncodedLen for FinalisedTxOutSetInfoAccumulator {
    /// 8 + 8 + 8 + 32 + 8 = 64 bytes.
    const ENCODED_LEN: usize = 8 + 8 + 8 + 32 + 8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalised_tx_out_set_info_accumulator_roundtrips() {
        let accumulator = FinalisedTxOutSetInfoAccumulator {
            transactions: 12,
            transaction_outputs: 34,
            bytes_serialized: 34 * ZAINO_TXOUTSET_ENTRY_LEN,
            hash_serialized: [0xab; 32],
            total_zatoshis: 1_234_567_890,
        };

        let encoded_accumulator = accumulator
            .to_bytes()
            .expect("finalised txout set info accumulator should encode");

        assert_eq!(
            encoded_accumulator.len(),
            FinalisedTxOutSetInfoAccumulator::VERSIONED_LEN
        );

        let decoded_accumulator =
            FinalisedTxOutSetInfoAccumulator::from_bytes(&encoded_accumulator)
                .expect("finalised txout set info accumulator should decode");

        assert_eq!(decoded_accumulator, accumulator);
    }

    #[test]
    fn finalised_tx_out_set_info_accumulator_empty_is_zero() {
        let accumulator = FinalisedTxOutSetInfoAccumulator::empty();

        assert_eq!(accumulator.transactions, 0);
        assert_eq!(accumulator.transaction_outputs, 0);
        assert_eq!(accumulator.bytes_serialized, 0);
        assert_eq!(accumulator.hash_serialized, [0u8; 32]);
        assert_eq!(accumulator.total_zatoshis, 0);
    }

    #[test]
    fn tx_out_set_entry_digest_xor_is_self_inverse() {
        let outpoint = Outpoint::new([7u8; 32], 3);
        let out = TxOutCompact::new(1_000_000, [0x11; 20], 0)
            .expect("script_type 0 (P2PKH) should be valid");
        let digest = tx_out_set_entry_digest(&outpoint, &out);

        let mut acc = [0u8; 32];
        for (dst, src) in acc.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
        for (dst, src) in acc.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
        assert_eq!(acc, [0u8; 32]);
    }

    #[test]
    fn tx_out_set_entry_digest_is_deterministic_and_domain_separated() {
        let outpoint = Outpoint::new([1u8; 32], 0);
        let out =
            TxOutCompact::new(42, [0x22; 20], 1).expect("script_type 1 (P2SH) should be valid");
        let a = tx_out_set_entry_digest(&outpoint, &out);
        let b = tx_out_set_entry_digest(&outpoint, &out);
        assert_eq!(a, b);

        // A naive un-tagged hash over the same bytes would be different.
        // We just sanity-check the digest is not all zeros.
        assert_ne!(a, [0u8; 32]);
    }

    #[test]
    fn is_unspendable_filters_non_standard() {
        let out = TxOutCompact::new(100, [0x00; 20], 0xFF)
            .expect("script_type 0xFF (NonStandard) should be valid");
        assert!(is_unspendable_tx_out(&out));
    }

    #[test]
    fn is_unspendable_allows_p2pkh() {
        let out =
            TxOutCompact::new(100, [0x00; 20], 0).expect("script_type 0 (P2PKH) should be valid");
        assert!(!is_unspendable_tx_out(&out));
    }

    #[test]
    fn is_unspendable_allows_p2sh() {
        let out =
            TxOutCompact::new(100, [0x00; 20], 1).expect("script_type 1 (P2SH) should be valid");
        assert!(!is_unspendable_tx_out(&out));
    }

    #[test]
    fn apply_added_then_removed_output_returns_to_empty() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let outpoint = Outpoint::new([0xAA; 32], 0);
        let out = TxOutCompact::new(50_000, [0x11; 20], 0)
            .expect("script_type 0 (P2PKH) should be valid");

        acc.apply_added_output(&outpoint, &out)
            .expect("add should succeed");

        assert_ne!(acc, FinalisedTxOutSetInfoAccumulator::empty());
        assert_eq!(acc.total_zatoshis, 50_000);
        assert_eq!(acc.transaction_outputs, 1);
        assert_eq!(acc.bytes_serialized, ZAINO_TXOUTSET_ENTRY_LEN);

        acc.apply_removed_output(&outpoint, &out)
            .expect("remove should succeed");

        assert_eq!(acc, FinalisedTxOutSetInfoAccumulator::empty());
    }

    #[test]
    fn apply_removed_output_on_empty_underflows() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let outpoint = Outpoint::new([0xBB; 32], 0);
        let out = TxOutCompact::new(1_000, [0x22; 20], 0)
            .expect("script_type 0 (P2PKH) should be valid");

        let err = acc
            .apply_removed_output(&outpoint, &out)
            .expect_err("remove on empty should underflow");

        assert_eq!(
            err,
            AccumulatorDeltaError::Underflow("transaction_outputs")
        );
    }

    #[test]
    fn apply_added_output_accumulates_values() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();

        let outpoint_a = Outpoint::new([0x01; 32], 0);
        let out_a = TxOutCompact::new(100, [0x11; 20], 0)
            .expect("script_type 0 (P2PKH) should be valid");

        let outpoint_b = Outpoint::new([0x02; 32], 1);
        let out_b = TxOutCompact::new(200, [0x22; 20], 1)
            .expect("script_type 1 (P2SH) should be valid");

        acc.apply_added_output(&outpoint_a, &out_a)
            .expect("add a should succeed");
        acc.apply_added_output(&outpoint_b, &out_b)
            .expect("add b should succeed");

        assert_eq!(acc.total_zatoshis, 300);
        assert_eq!(acc.transaction_outputs, 2);
        assert_eq!(acc.bytes_serialized, 2 * ZAINO_TXOUTSET_ENTRY_LEN);

        let digest_a = tx_out_set_entry_digest(&outpoint_a, &out_a);
        let digest_b = tx_out_set_entry_digest(&outpoint_b, &out_b);
        let mut expected_hash = [0u8; 32];
        for i in 0..32 {
            expected_hash[i] = digest_a[i] ^ digest_b[i];
        }
        assert_eq!(acc.hash_serialized, expected_hash);
    }
}
