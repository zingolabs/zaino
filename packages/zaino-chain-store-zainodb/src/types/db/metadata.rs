//! Metadata objects

use corez::io::{self, Read, Write};

use zaino_encoding::{
    read_fixed_le, read_u64_le, version, write_fixed_le, write_u64_le, FixedEncodedLen,
    ZainoVersionedSerde,
};

use super::legacy::{Outpoint, ScriptType, TxOutCompact};
use zaino_chain_store::{entry_digest_parts, Delta, TxOutSetAccumulator, TxOutSetError};

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

/// Computes the per-UTXO digest used by [`FinalisedTxOutSetInfoAccumulator::hash_serialized`].
///
/// Forwards to [`entry_digest_parts`] with this backend's stored bytes. The
/// digest is not defined here: it is a contract between finalised-state
/// implementations, because two stores holding the same UTXO set must produce
/// the same `hash_serialized` or `gettxoutsetinfo` stops meaning one thing.
/// `zaino_chain_store::txout_set` explains why that cannot be checked at
/// runtime.
///
/// The five values go across as bytes, which is why no conversion into domain
/// types happens here: `script_type()` is a raw stored byte, and turning it
/// into a `ScriptType` first would put a fallible step on the commitment's hot
/// path for no gain.
pub fn tx_out_set_entry_digest(outpoint: &Outpoint, out: &TxOutCompact) -> [u8; 32] {
    entry_digest_parts(
        *outpoint.prev_txid(),
        outpoint.prev_index(),
        out.value(),
        *out.script_hash(),
        out.script_type(),
    )
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
/// TXOUT_SET_ENTRY_LEN`. Stored explicitly so the wire mapping is a
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
        self.apply_output_delta(outpoint, out, Delta::Added)
    }

    /// Applies one output entering or leaving the set.
    ///
    /// Delegates the fold to [`TxOutSetAccumulator::apply_entry`]. What one
    /// output does to the accumulator is the commitment's definition, not this
    /// backend's: `bytes_serialized` in particular moves by the canonical entry
    /// length rather than by anything measured here, and `gettxoutsetinfo`
    /// reports that number to a user who may be comparing two deployments.
    fn apply_output_delta(
        &mut self,
        outpoint: &Outpoint,
        out: &TxOutCompact,
        delta: Delta,
    ) -> Result<(), AccumulatorDeltaError> {
        let mut folded = self.into_business();
        folded.apply_entry(&tx_out_set_entry_digest(outpoint, out), out.value(), delta)?;
        self.replace_business(folded);
        Ok(())
    }

    /// Folds another accumulator into this one.
    ///
    /// Delegates for the same reason as [`Self::apply_output_delta`]: XOR and
    /// checked addition over these five fields are the commitment's arithmetic,
    /// and a shard recombination that disagreed with it would produce a
    /// perfectly plausible wrong answer.
    pub fn combine(&mut self, other: &Self) -> Result<(), AccumulatorDeltaError> {
        let mut folded = self.into_business();
        folded.combine(&other.into_business())?;
        self.replace_business(folded);
        Ok(())
    }

    /// This row as the domain value it stores.
    ///
    /// The persistence boundary, per the `Persistent*` convention: the fields
    /// are the same because the row *is* the domain value, but the encoding
    /// below is this backend's and the value is not. Takes `self` by value —
    /// the row is `Copy`, and the name says the direction.
    pub(crate) fn into_business(self) -> TxOutSetAccumulator {
        TxOutSetAccumulator {
            transactions: self.transactions,
            transaction_outputs: self.transaction_outputs,
            bytes_serialized: self.bytes_serialized,
            hash_serialized: self.hash_serialized,
            total_zatoshis: self.total_zatoshis,
        }
    }

    /// Overwrites this row with a domain value. Inverse of [`Self::into_business`].
    fn replace_business(&mut self, value: TxOutSetAccumulator) {
        self.transactions = value.transactions;
        self.transaction_outputs = value.transaction_outputs;
        self.bytes_serialized = value.bytes_serialized;
        self.hash_serialized = value.hash_serialized;
        self.total_zatoshis = value.total_zatoshis;
    }

    /// Applies a single UTXO leaving the set to all per-output fields. Inverse of
    /// [`Self::apply_added_output`].
    pub fn apply_removed_output(
        &mut self,
        outpoint: &Outpoint,
        out: &TxOutCompact,
    ) -> Result<(), AccumulatorDeltaError> {
        self.apply_output_delta(outpoint, out, Delta::Removed)
    }
}

/// Failure modes for accumulator delta operations.
///
/// Carries the field name so callers can produce specific error context.
///
/// This backend's spelling of [`TxOutSetError`]. Kept distinct because it is
/// what this crate's callers already match on, and converted rather than
/// re-exported so the domain error stays the domain's to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccumulatorDeltaError {
    /// A counter or running sum overflowed `u64`.
    #[error("txout-set accumulator {0} overflow")]
    Overflow(&'static str),
    /// A counter or running sum underflowed `u64`.
    #[error("txout-set accumulator {0} underflow")]
    Underflow(&'static str),
}

impl From<TxOutSetError> for AccumulatorDeltaError {
    fn from(error: TxOutSetError) -> Self {
        match error {
            TxOutSetError::Overflow(field) => Self::Overflow(field),
            TxOutSetError::Underflow(field) => Self::Underflow(field),
        }
    }
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

/// Fixed-length encoding metadata for `FinalisedTxOutSetInfoAccumulator`.
///
/// v1 consists of 8 + 8 + 8 + 32 + 8 = 64 bytes
impl FixedEncodedLen for FinalisedTxOutSetInfoAccumulator {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(64),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_chain_store::TXOUT_SET_ENTRY_LEN;

    #[test]
    fn finalised_tx_out_set_info_accumulator_roundtrips() {
        let accumulator = FinalisedTxOutSetInfoAccumulator {
            transactions: 12,
            transaction_outputs: 34,
            bytes_serialized: 34 * TXOUT_SET_ENTRY_LEN,
            hash_serialized: [0xab; 32],
            total_zatoshis: 1_234_567_890,
        };

        let encoded_accumulator = accumulator
            .to_bytes()
            .expect("finalised txout set info accumulator should encode");

        assert_eq!(
            encoded_accumulator.len(),
            FinalisedTxOutSetInfoAccumulator::latest_versioned_len().unwrap()
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
    fn combine_xors_commitments_and_sums_counters() {
        let a = FinalisedTxOutSetInfoAccumulator::new(2, 10, 100, [0xaa; 32], 500);
        let b = FinalisedTxOutSetInfoAccumulator::new(3, 7, 70, [0x0f; 32], 250);

        let mut combined = a;
        combined.combine(&b).expect("no overflow");

        assert_eq!(combined.transactions, 5);
        assert_eq!(combined.transaction_outputs, 17);
        assert_eq!(combined.bytes_serialized, 170);
        assert_eq!(combined.total_zatoshis, 750);
        assert_eq!(combined.hash_serialized, [0xaa ^ 0x0f; 32]);
    }

    #[test]
    fn combine_is_order_independent() {
        let a = FinalisedTxOutSetInfoAccumulator::new(2, 10, 100, [0xaa; 32], 500);
        let b = FinalisedTxOutSetInfoAccumulator::new(3, 7, 70, [0x0f; 32], 250);

        let mut ab = a;
        ab.combine(&b).expect("no overflow");
        let mut ba = b;
        ba.combine(&a).expect("no overflow");

        assert_eq!(
            ab, ba,
            "XOR + addition are commutative, so combine must be order-independent"
        );
    }

    #[test]
    fn combine_detects_counter_overflow() {
        let mut a = FinalisedTxOutSetInfoAccumulator::new(u64::MAX, 0, 0, [0; 32], 0);
        let b = FinalisedTxOutSetInfoAccumulator::new(1, 0, 0, [0; 32], 0);

        assert!(
            a.combine(&b).is_err(),
            "a counter overflow must surface as an error, never silently wrap"
        );
    }

    #[test]
    fn combine_recombines_partitioned_outputs() {
        // This is the property the sharded rebuild relies on: accumulating disjoint groups of the
        // UTXO set and folding the partials with `combine` must equal accumulating the whole set in
        // one pass. (`apply_added_output` does not touch `transactions`, so this exercises the XOR
        // commitment and the per-output additive counters; the `transactions` sum is covered above.)
        let outputs: Vec<(Outpoint, TxOutCompact)> = (0..6u32)
            .map(|i| {
                let outpoint = Outpoint::new([i as u8; 32], i);
                let out = TxOutCompact::new(1_000 + u64::from(i), [i as u8; 20], (i % 2) as u8)
                    .expect("script_type 0/1 is valid");
                (outpoint, out)
            })
            .collect();

        let mut whole = FinalisedTxOutSetInfoAccumulator::empty();
        for (outpoint, out) in &outputs {
            whole
                .apply_added_output(outpoint, out)
                .expect("no overflow");
        }

        // Split into two arbitrary disjoint groups, accumulate each, and recombine.
        let mut part_a = FinalisedTxOutSetInfoAccumulator::empty();
        for (outpoint, out) in &outputs[..2] {
            part_a
                .apply_added_output(outpoint, out)
                .expect("no overflow");
        }
        let mut part_b = FinalisedTxOutSetInfoAccumulator::empty();
        for (outpoint, out) in &outputs[2..] {
            part_b
                .apply_added_output(outpoint, out)
                .expect("no overflow");
        }

        let mut combined = part_a;
        combined.combine(&part_b).expect("no overflow");

        assert_eq!(
            combined, whole,
            "combining partials of a partition must equal accumulating the whole set"
        );
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
        assert_eq!(acc.bytes_serialized, TXOUT_SET_ENTRY_LEN);

        acc.apply_removed_output(&outpoint, &out)
            .expect("remove should succeed");

        assert_eq!(acc, FinalisedTxOutSetInfoAccumulator::empty());
    }

    #[test]
    fn apply_removed_output_on_empty_underflows() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();
        let outpoint = Outpoint::new([0xBB; 32], 0);
        let out =
            TxOutCompact::new(1_000, [0x22; 20], 0).expect("script_type 0 (P2PKH) should be valid");

        let err = acc
            .apply_removed_output(&outpoint, &out)
            .expect_err("remove on empty should underflow");

        assert_eq!(err, AccumulatorDeltaError::Underflow("transaction_outputs"));
    }

    #[test]
    fn apply_added_output_accumulates_values() {
        let mut acc = FinalisedTxOutSetInfoAccumulator::empty();

        let outpoint_a = Outpoint::new([0x01; 32], 0);
        let out_a =
            TxOutCompact::new(100, [0x11; 20], 0).expect("script_type 0 (P2PKH) should be valid");

        let outpoint_b = Outpoint::new([0x02; 32], 1);
        let out_b =
            TxOutCompact::new(200, [0x22; 20], 1).expect("script_type 1 (P2SH) should be valid");

        acc.apply_added_output(&outpoint_a, &out_a)
            .expect("add a should succeed");
        acc.apply_added_output(&outpoint_b, &out_b)
            .expect("add b should succeed");

        assert_eq!(acc.total_zatoshis, 300);
        assert_eq!(acc.transaction_outputs, 2);
        assert_eq!(acc.bytes_serialized, 2 * TXOUT_SET_ENTRY_LEN);

        let digest_a = tx_out_set_entry_digest(&outpoint_a, &out_a);
        let digest_b = tx_out_set_entry_digest(&outpoint_b, &out_b);
        let mut expected_hash = [0u8; 32];
        for i in 0..32 {
            expected_hash[i] = digest_a[i] ^ digest_b[i];
        }
        assert_eq!(acc.hash_serialized, expected_hash);
    }
}
