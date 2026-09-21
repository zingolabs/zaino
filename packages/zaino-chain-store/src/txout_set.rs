//! Zaino's transparent UTXO-set commitment, and the running totals beside it.
//!
//! # Why this is a domain contract and not a backend detail
//!
//! `hash_serialized` is a multiset commitment: the XOR of a per-output digest
//! over every currently-unspent transparent output. XOR makes it self-inverse
//! and order-independent, which is what lets a store maintain it incrementally
//! and lets a consumer extend it across the finalised/recent seam without
//! replaying history.
//!
//! That last property is why the commitment lives here rather than inside a
//! backend. Serving `gettxoutsetinfo` at the chain tip means taking the
//! store's finalised accumulator and applying the outputs the recent window
//! has created and spent — so whoever merges must compute the *same* digest
//! the store did. A commitment scheme that differed between backends would
//! make the value mean different things on different deployments, and a
//! consumer could not merge across the seam at all.
//!
//! So the canonical entry encoding below is a contract every finalised-state
//! adapter conforms to, not an encoding any one of them owns. It is fixed, and
//! changing it changes what the value means.
//!
//! # What an adapter is and is not required to do
//!
//! Three things here are contracts, because each shows up in a number a user
//! can compare between two Zaino deployments:
//!
//! 1. [`TXOUT_SET_DOMAIN_TAG`], which separates this hash from every other
//!    BLAKE2b-256 Zaino computes;
//! 2. the field order and widths of [`canonical_entry_parts`] — change any of
//!    them and `hash_serialized` changes for the same UTXO set;
//! 3. [`TXOUT_SET_ENTRY_LEN`], which is *not* a storage size. It is multiplied
//!    into `bytes_serialized`, which `gettxoutsetinfo` reports verbatim, so two
//!    stores that disagree on it report different byte counts for one chain.
//!
//! None of that constrains how an adapter stores anything. The canonical entry
//! is a hash preimage: it is built, hashed, and dropped, and is never written
//! to disk. An adapter is free to persist its accumulator in whatever encoding
//! it likes — and this crate deliberately does not define one.
//!
//! What an adapter *is* required to do is be able to **produce** the five
//! values the preimage is made of, per unspent output. That carries a real
//! limitation worth naming rather than leaving to be discovered: the
//! commitment bakes in Zaino's lossy transparent encoding, a 20-byte address
//! hash plus a one-byte script tag. An adapter that stored whole
//! `script_pubkey`s would still have to reduce each one to that pair to
//! compute the commitment. That constraint is a property of the commitment as
//! designed, not of this crate layout, and it is fixed for the same reason
//! everything else here is.
//!
//! # Why none of this can be caught at runtime
//!
//! Two adapters that disagree do not fail. They return different numbers for
//! the same chain, and each is internally consistent. There is no read that
//! notices, and no error to propagate — which is precisely why the definition
//! is here, in the crate both adapters already depend on, rather than copied
//! into each.
//!
//! # What it is not
//!
//! Not byte-equal to zcashd's `hash_serialized`, and not intended to be.
//! Zaino commits to its own representation of the UTXO set.

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use zaino_primitives::types::{Outpoint, ScriptType};

use crate::output::StoredTxOut;

/// Domain separator for the commitment.
///
/// Prepended to every entry before hashing so this digest cannot collide with
/// any other BLAKE2b-256 Zaino computes.
pub const TXOUT_SET_DOMAIN_TAG: &[u8; 16] = b"ZcashTxOutSet___";

/// Canonical encoded length of one UTXO entry, in bytes.
///
/// `txid(32) + output_index(4) + value(8) + address_hash(20) + script_tag(1)`.
/// `bytes_serialized` is this multiplied by the number of unspent outputs, so
/// it is a property of the commitment rather than of any storage layout.
pub const TXOUT_SET_ENTRY_LEN: u64 = 32 + 4 + 8 + 20 + 1;

/// The commitment's tag for a script form.
///
/// Part of the canonical entry encoding, and therefore fixed. Deliberately
/// **not** discriminants on [`ScriptType`] itself: that type is shared
/// vocabulary and must not carry one scheme's byte values, and a backend is
/// free to tag its own rows differently so long as it maps to these when
/// computing the digest.
pub const fn script_type_tag(script_type: ScriptType) -> u8 {
    match script_type {
        ScriptType::P2PKH => 0x00,
        ScriptType::P2SH => 0x01,
        ScriptType::NonStandard => 0xFF,
    }
}

/// Whether an output is excluded from the UTXO set.
///
/// Mirrors zcashd's `IsUnspendable` for `gettxoutsetinfo`: only outputs whose
/// script parses as P2PKH or P2SH are counted. Everything else — OP_RETURN
/// coinbase commitments, oversized or otherwise non-standard scripts — is
/// treated as unspendable and contributes to no field of the accumulator.
///
/// Note this differs from what an address index does with the same outputs:
/// Zaino's transparent history keys non-standard outputs, because a client
/// asking about them wants an answer. The two subsystems disagree
/// deliberately, and each states which rule it follows.
pub fn is_unspendable(out: &StoredTxOut) -> bool {
    !matches!(
        out.address.script_type,
        ScriptType::P2PKH | ScriptType::P2SH
    )
}

/// The canonical bytes committed to for one unspent output.
///
/// Exposed so the layout can be asserted directly rather than only through the
/// digest: a change here is a change to the contract, and a test that only
/// checked the hash would report it as an opaque mismatch.
pub fn canonical_entry(
    outpoint: &Outpoint,
    out: &StoredTxOut,
) -> [u8; TXOUT_SET_ENTRY_LEN as usize] {
    canonical_entry_parts(
        outpoint.txid.into(),
        outpoint.index,
        u64::from(out.value),
        out.address.hash,
        script_type_tag(out.address.script_type),
    )
}

/// The canonical entry, from the five values it is made of.
///
/// The contract itself. [`canonical_entry`] is the convenience form for a
/// caller that already holds domain types; this is the form an adapter uses,
/// because an adapter holds its own stored representation and converting it
/// into domain types purely to hash it would add a conversion — and, where the
/// script tag is stored as a raw byte, a *fallible* one — to the commitment's
/// hot path. The commitment is the last place to introduce a new way to fail.
///
/// `script_tag` is the value [`script_type_tag`] produces; an adapter that
/// stores that byte directly passes it through unchanged.
pub fn canonical_entry_parts(
    txid: [u8; 32],
    index: u32,
    value: u64,
    address_hash: [u8; 20],
    script_tag: u8,
) -> [u8; TXOUT_SET_ENTRY_LEN as usize] {
    let mut entry = [0u8; TXOUT_SET_ENTRY_LEN as usize];
    entry[..32].copy_from_slice(&txid);
    entry[32..36].copy_from_slice(&index.to_le_bytes());
    entry[36..44].copy_from_slice(&value.to_le_bytes());
    entry[44..64].copy_from_slice(&address_hash);
    entry[64] = script_tag;
    entry
}

/// The per-output digest XORed into the commitment.
///
/// `BLAKE2b-256(TXOUT_SET_DOMAIN_TAG || canonical_entry(outpoint, out))`.
/// XORed in on add and again on remove — XOR being self-inverse is what makes
/// removal exact rather than approximate.
pub fn entry_digest(outpoint: &Outpoint, out: &StoredTxOut) -> [u8; 32] {
    entry_digest_parts(
        outpoint.txid.into(),
        outpoint.index,
        u64::from(out.value),
        out.address.hash,
        script_type_tag(out.address.script_type),
    )
}

/// The per-output digest, from the five values the entry is made of.
///
/// The form an adapter calls, for the reason given on
/// [`canonical_entry_parts`]. This is the only place the digest is computed:
/// an adapter that hashed the same fields itself would agree with this one
/// only for as long as nobody edited either, and nothing would report the
/// disagreement.
pub fn entry_digest_parts(
    txid: [u8; 32],
    index: u32,
    value: u64,
    address_hash: [u8; 20],
    script_tag: u8,
) -> [u8; 32] {
    let mut hasher =
        Blake2bVar::new(32).expect("BLAKE2b-256 initialises with a 32-byte digest size");
    hasher.update(TXOUT_SET_DOMAIN_TAG);
    hasher.update(&canonical_entry_parts(
        txid,
        index,
        value,
        address_hash,
        script_tag,
    ));
    let mut digest = [0u8; 32];
    hasher
        .finalize_variable(&mut digest)
        .expect("BLAKE2b-256 finalises into a matching digest size");
    digest
}

/// Running totals over the unspent transparent output set.
///
/// A *partial* fold, not an RPC answer: it describes the set as of some
/// height, and a consumer serving `gettxoutsetinfo` at the tip extends it with
/// what the recent window has changed. The finished answer is
/// [`TxOutSetInfo`](zaino_primitives::types::TxOutSetInfo).
///
/// Every field is maintained incrementally. `transactions` is the exception a
/// caller must handle: it counts transactions with *at least one* unspent
/// output, which cannot be decided from a single output, so
/// [`Self::apply_added_output`] and [`Self::apply_removed_output`] leave it
/// alone and the caller steps it on the 0↔non-zero transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxOutSetAccumulator {
    /// Transactions with at least one currently-unspent transparent output.
    pub transactions: u64,
    /// Currently-unspent transparent outputs.
    pub transaction_outputs: u64,
    /// Canonical byte-length of the set: `transaction_outputs * TXOUT_SET_ENTRY_LEN`.
    pub bytes_serialized: u64,
    /// The multiset commitment.
    pub hash_serialized: [u8; 32],
    /// Summed value of every currently-unspent transparent output.
    pub total_zatoshis: u64,
}

impl TxOutSetAccumulator {
    /// An accumulator over the empty set.
    ///
    /// The commitment over no outputs is all zeroes, which is XOR's identity —
    /// so an empty accumulator combined with any other is that other.
    pub const fn empty() -> Self {
        Self {
            transactions: 0,
            transaction_outputs: 0,
            bytes_serialized: 0,
            hash_serialized: [0u8; 32],
            total_zatoshis: 0,
        }
    }

    /// Applies one output entering the set.
    ///
    /// Does not touch `transactions` — see the type's documentation.
    pub fn apply_added_output(
        &mut self,
        outpoint: &Outpoint,
        out: &StoredTxOut,
    ) -> Result<(), TxOutSetError> {
        self.apply_delta(outpoint, out, Delta::Added)
    }

    /// Applies one output leaving the set. Exact inverse of
    /// [`Self::apply_added_output`].
    pub fn apply_removed_output(
        &mut self,
        outpoint: &Outpoint,
        out: &StoredTxOut,
    ) -> Result<(), TxOutSetError> {
        self.apply_delta(outpoint, out, Delta::Removed)
    }

    /// The per-output fold, over an already-computed digest and value.
    ///
    /// The definition of what one output does to the accumulator: XOR its
    /// digest into the commitment, then step each per-output counter in the
    /// delta's direction. `bytes_serialized` moves by
    /// [`TXOUT_SET_ENTRY_LEN`] rather than by anything an adapter measures,
    /// which is what keeps the reported byte count a property of the
    /// commitment rather than of a storage layout.
    ///
    /// Takes the digest rather than computing it so an adapter can hash from
    /// its own representation via [`entry_digest_parts`] without first
    /// converting into domain types. `transactions` is untouched — see the
    /// type's documentation.
    pub fn apply_entry(
        &mut self,
        digest: &[u8; 32],
        value: u64,
        delta: Delta,
    ) -> Result<(), TxOutSetError> {
        self.xor_in(digest);
        self.transaction_outputs =
            delta.step(self.transaction_outputs, 1, "transaction_outputs")?;
        self.bytes_serialized = delta.step(
            self.bytes_serialized,
            TXOUT_SET_ENTRY_LEN,
            "bytes_serialized",
        )?;
        self.total_zatoshis = delta.step(self.total_zatoshis, value, "total_zatoshis")?;
        Ok(())
    }

    /// Folds another accumulator into this one.
    ///
    /// XOR and addition are both commutative and associative, so a set split
    /// into shards and recombined gives the same result regardless of how many
    /// shards there were or what order they were folded in. That is what makes
    /// a parallel rebuild safe.
    pub fn combine(&mut self, other: &Self) -> Result<(), TxOutSetError> {
        self.xor_in(&other.hash_serialized);
        self.transactions = checked(self.transactions, other.transactions, "transactions")?;
        self.transaction_outputs = checked(
            self.transaction_outputs,
            other.transaction_outputs,
            "transaction_outputs",
        )?;
        self.bytes_serialized = checked(
            self.bytes_serialized,
            other.bytes_serialized,
            "bytes_serialized",
        )?;
        self.total_zatoshis = checked(self.total_zatoshis, other.total_zatoshis, "total_zatoshis")?;
        Ok(())
    }

    fn apply_delta(
        &mut self,
        outpoint: &Outpoint,
        out: &StoredTxOut,
        delta: Delta,
    ) -> Result<(), TxOutSetError> {
        self.apply_entry(&entry_digest(outpoint, out), u64::from(out.value), delta)
    }

    fn xor_in(&mut self, digest: &[u8; 32]) {
        for (dst, src) in self.hash_serialized.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
    }
}

fn checked(a: u64, b: u64, field: &'static str) -> Result<u64, TxOutSetError> {
    a.checked_add(b).ok_or(TxOutSetError::Overflow(field))
}

/// Direction of a per-output update.
///
/// Public because the fold it drives is public: an adapter maintaining the
/// commitment names the direction, and [`TxOutSetAccumulator::apply_entry`]
/// applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    /// The output is entering the set.
    Added,
    /// The output is leaving the set.
    Removed,
}

impl Delta {
    fn step(self, field: u64, amount: u64, name: &'static str) -> Result<u64, TxOutSetError> {
        match self {
            Delta::Added => field
                .checked_add(amount)
                .ok_or(TxOutSetError::Overflow(name)),
            Delta::Removed => field
                .checked_sub(amount)
                .ok_or(TxOutSetError::Underflow(name)),
        }
    }
}

/// A counter left the range a `u64` can hold.
///
/// Names the field, because which one overflowed says what went wrong: an
/// underflow on `transaction_outputs` means an output was removed twice, where
/// one on `total_zatoshis` means a value was misread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TxOutSetError {
    /// A counter or running sum overflowed.
    #[error("txout-set accumulator {0} overflowed")]
    Overflow(&'static str),
    /// A counter or running sum underflowed.
    #[error("txout-set accumulator {0} underflowed")]
    Underflow(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{TransactionId, Zatoshis};

    use crate::output::StoredAddress;

    fn outpoint() -> Outpoint {
        Outpoint {
            txid: TransactionId::from([0x11; 32]),
            index: 7,
        }
    }

    fn output(script_type: ScriptType) -> StoredTxOut {
        StoredTxOut::new(
            Zatoshis::new(21_000_000).expect("within range"),
            StoredAddress::new([0xbb; 20], script_type),
        )
    }

    /// The canonical entry layout, asserted byte by byte.
    ///
    /// This is the contract. Every finalised-state implementation commits to
    /// these bytes, and whatever merges finalised and recent answers recomputes
    /// them — so a change here silently changes what `hash_serialized` means
    /// rather than breaking anything. Written out by hand rather than derived,
    /// so that the test disagrees with the code when the code changes.
    #[test]
    fn the_canonical_entry_layout_is_fixed() {
        let entry = canonical_entry(&outpoint(), &output(ScriptType::P2PKH));

        assert_eq!(&entry[..32], &[0x11; 32], "txid, as stored");
        assert_eq!(&entry[32..36], &[0x07, 0x00, 0x00, 0x00], "index, LE u32");
        assert_eq!(
            &entry[36..44],
            &[0x40, 0x6f, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00],
            "value, LE u64 (21_000_000)"
        );
        assert_eq!(&entry[44..64], &[0xbb; 20], "address hash");
        assert_eq!(entry[64], 0x00, "script tag, P2PKH");
        assert_eq!(entry.len() as u64, TXOUT_SET_ENTRY_LEN);
    }

    /// The tags are fixed values, not whatever the enum happens to order to.
    #[test]
    fn the_script_tags_are_fixed() {
        assert_eq!(script_type_tag(ScriptType::P2PKH), 0x00);
        assert_eq!(script_type_tag(ScriptType::P2SH), 0x01);
        assert_eq!(script_type_tag(ScriptType::NonStandard), 0xFF);
    }

    /// Adding then removing the same output returns the accumulator to empty.
    ///
    /// The property the whole scheme rests on: removal is exact, so a store
    /// can maintain the commitment incrementally instead of recomputing it,
    /// and a consumer can extend it across the seam and take it back again.
    #[test]
    fn removal_exactly_undoes_addition() {
        let mut acc = TxOutSetAccumulator::empty();
        acc.apply_added_output(&outpoint(), &output(ScriptType::P2PKH))
            .expect("add");
        assert_ne!(acc, TxOutSetAccumulator::empty());

        acc.apply_removed_output(&outpoint(), &output(ScriptType::P2PKH))
            .expect("remove");
        assert_eq!(acc, TxOutSetAccumulator::empty());
    }

    /// Order does not change the commitment.
    ///
    /// XOR is commutative, which is what allows a parallel or sharded rebuild
    /// to produce the same value as a sequential one.
    #[test]
    fn the_commitment_is_order_independent() {
        let a = outpoint();
        let b = Outpoint {
            txid: TransactionId::from([0x22; 32]),
            index: 1,
        };

        let mut forwards = TxOutSetAccumulator::empty();
        forwards
            .apply_added_output(&a, &output(ScriptType::P2PKH))
            .expect("add");
        forwards
            .apply_added_output(&b, &output(ScriptType::P2SH))
            .expect("add");

        let mut backwards = TxOutSetAccumulator::empty();
        backwards
            .apply_added_output(&b, &output(ScriptType::P2SH))
            .expect("add");
        backwards
            .apply_added_output(&a, &output(ScriptType::P2PKH))
            .expect("add");

        assert_eq!(forwards, backwards);
    }

    /// Combining shards equals accumulating in one pass.
    #[test]
    fn shards_recombine_to_the_whole() {
        let a = outpoint();
        let b = Outpoint {
            txid: TransactionId::from([0x22; 32]),
            index: 1,
        };

        let mut whole = TxOutSetAccumulator::empty();
        whole
            .apply_added_output(&a, &output(ScriptType::P2PKH))
            .expect("add");
        whole
            .apply_added_output(&b, &output(ScriptType::P2SH))
            .expect("add");

        let mut first = TxOutSetAccumulator::empty();
        first
            .apply_added_output(&a, &output(ScriptType::P2PKH))
            .expect("add");
        let mut second = TxOutSetAccumulator::empty();
        second
            .apply_added_output(&b, &output(ScriptType::P2SH))
            .expect("add");
        first.combine(&second).expect("combine");

        assert_eq!(first, whole);
    }

    /// Removing from an empty set underflows rather than wrapping.
    #[test]
    fn removing_what_was_never_added_underflows() {
        let mut acc = TxOutSetAccumulator::empty();
        let error = acc
            .apply_removed_output(&outpoint(), &output(ScriptType::P2PKH))
            .expect_err("must not wrap");
        assert!(matches!(error, TxOutSetError::Underflow(_)));
    }

    /// Only standard outputs are in the set.
    #[test]
    fn non_standard_outputs_are_unspendable() {
        assert!(!is_unspendable(&output(ScriptType::P2PKH)));
        assert!(!is_unspendable(&output(ScriptType::P2SH)));
        assert!(is_unspendable(&output(ScriptType::NonStandard)));
    }

    /// The digest, pinned.
    ///
    /// Minted from this implementation and cross-checked against the finalised
    /// state's existing `tx_out_set_entry_digest` over the same input, so it
    /// records agreement with what is already committed on disk rather than
    /// merely agreement with itself.
    #[test]
    fn the_entry_digest_is_pinned() {
        assert_eq!(
            hex_of(&entry_digest(&outpoint(), &output(ScriptType::P2PKH))),
            "86b0419f85f42591be0b2992470dee1d88f38c545825a9fbcf9890f7eaa8fda9",
        );
    }

    fn hex_of(bytes: &[u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
