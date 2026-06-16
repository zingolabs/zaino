//! In-memory transparent UTXO cache: the txout-set accumulator's source of
//! spent-output values and per-transaction unspent counts.
//!
//! # Why
//!
//! `calculate_tx_out_set_info_accumulator_after_block` needs, per spent transparent
//! input, the spent output's value and its source transaction's remaining-unspent
//! count. Reading those from disk means, per block, random B+tree page faults on the
//! `transparent`, `txid_location`, and `spent` tables — and once those indexes exceed
//! RAM that is the dominant cost of the sync slowdown (see the
//! `project_sync_write_path_read_audit` memory).
//!
//! A *forward* sync, though, observes every transparent output — value included
//! (`TxOutCompact` carries `value`) — at the moment it ingests the creating block, and
//! every output is created before it is spent. So this cache carries that information
//! forward in memory and serves both answers ([`value_of`], [`remaining_unspent`])
//! without touching disk.
//!
//! # What this holds
//!
//! The live *unspent* transparent UTXO set:
//!
//! - `outputs`:        outpoint → its unspent output (the value).  [value resolution]
//! - `unspent_per_tx`: txid     → count of its still-unspent outputs.  [tx count]
//!
//! # Lifecycle
//!
//! Purely in-memory *derived* state: no schema change, nothing new persisted, and
//! reconstructable from the committed `transparent` and `spent` tables at any time.
//!
//! - **Seed on open** (`DbV1::seed_transparent_utxo_cache`): two sequential scans of
//!   the committed tables rebuild the unspent set — a no-op at genesis, a one-time scan
//!   on resume before the first new block.
//! - **Maintain at build time** ([`apply_forward`]): the block-write path records
//!   created outputs and removes spent ones as it builds each block, so the accumulator
//!   reads the pre-block state. A write that does not durably land reseeds from committed
//!   state.
//! - **Reset is re-derive-forward**: there is no in-place reverse. A rollback (V1 does
//!   not support tip deletion) reseeds via [`clear`] + seed, matching the append-only
//!   design (`docs/decision_records/finalised_state/append_only_design.md`).
//!
//! [`value_of`]: TransparentUtxoCache::value_of
//! [`remaining_unspent`]: TransparentUtxoCache::remaining_unspent
//! [`apply_forward`]: TransparentUtxoCache::apply_forward
//! [`clear`]: TransparentUtxoCache::clear

use std::sync::Arc;

use dashmap::DashMap;

use crate::chain_index::types::db::metadata::is_unspendable_tx_out;
use crate::chain_index::types::{Outpoint, TransactionHash, TxOutCompact};

/// The live unspent transparent UTXO set, held in memory and maintained forward
/// as blocks are ingested. Cheap to clone (`Arc`-backed, matching the caches it
/// replaces).
#[derive(Clone, Debug, Default)]
pub(super) struct TransparentUtxoCache {
    /// Unspent outpoint → its output (carries the value the accumulator needs).
    outputs: Arc<DashMap<Outpoint, TxOutCompact>>,
    /// Txid → number of its outputs still unspent. Subsumes
    /// `DbV1::unspent_output_counts`.
    unspent_per_tx: Arc<DashMap<TransactionHash, u32>>,
}

impl TransparentUtxoCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Number of unspent transparent outputs currently held.
    pub(super) fn len(&self) -> usize {
        self.outputs.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Rough resident-byte estimate of the cache, for sync-time telemetry that sizes
    /// the live UTXO set against the write-batch budget. Not exact: each map's entry
    /// count times its payload (`outputs`: `Outpoint` 36 B + `TxOutCompact` 32 B;
    /// `unspent_per_tx`: `TransactionHash` 32 B + `u32` 4 B) inflated ~2× for hashbrown
    /// control bytes, the ≤7/8 load factor, and DashMap's power-of-two shard capacity.
    pub(super) fn estimated_resident_bytes(&self) -> usize {
        const OUTPUT_ENTRY_BYTES: usize = 150;
        const PER_TX_ENTRY_BYTES: usize = 80;
        self.outputs.len() * OUTPUT_ENTRY_BYTES + self.unspent_per_tx.len() * PER_TX_ENTRY_BYTES
    }

    /// Records an output a block created (insert), keeping the per-tx count.
    pub(super) fn record_created(&self, outpoint: Outpoint, output: TxOutCompact) {
        if self.outputs.insert(outpoint, output).is_none() {
            let txid = TransactionHash::from(*outpoint.prev_txid());
            *self.unspent_per_tx.entry(txid).or_insert(0) += 1;
        }
    }

    /// Records a spend (remove), keeping the per-tx count. Returns the spent
    /// output so the caller has its value with no database read.
    pub(super) fn record_spent(&self, outpoint: &Outpoint) -> Option<TxOutCompact> {
        let removed = self.outputs.remove(outpoint).map(|(_, output)| output);
        if removed.is_some() {
            let txid = TransactionHash::from(*outpoint.prev_txid());
            // Decrement the source tx's unspent count, dropping the entry at zero so the
            // map holds only transactions that still have unspent outputs (otherwise it
            // would grow one stale entry per fully-spent transaction over a long sync).
            if let dashmap::mapref::entry::Entry::Occupied(mut entry) =
                self.unspent_per_tx.entry(txid)
            {
                let count = entry.get_mut();
                *count = count.saturating_sub(1);
                if *count == 0 {
                    entry.remove();
                }
            }
        }
        removed
    }

    /// Applies a block's transparent delta forward, after its writes commit: records
    /// every spendable created output and removes every spent one. Creations are
    /// applied before spends so an output created and spent within the same block nets
    /// out. The single forward maintenance entry point.
    pub(super) fn apply_forward(&self, delta: &super::transparent_delta::TransparentBlockDelta) {
        for (outpoint, output, _location) in &delta.created {
            if !is_unspendable_tx_out(output) {
                self.record_created(*outpoint, *output);
            }
        }
        for (outpoint, _location) in &delta.spent {
            self.record_spent(outpoint);
        }
    }

    /// Empties the cache. Used before a reseed (e.g. after a block delete) so the
    /// reconstruction starts from a clean slate.
    pub(super) fn clear(&self) {
        self.outputs.clear();
        self.unspent_per_tx.clear();
    }

    /// The unspent output at `outpoint`, if still unspent. Replaces the
    /// `txid_location` + `transparent` reads.
    pub(super) fn value_of(&self, outpoint: &Outpoint) -> Option<TxOutCompact> {
        self.outputs.get(outpoint).map(|entry| *entry.value())
    }

    /// Count of `txid`'s outputs still unspent. Replaces the `spent` read used
    /// for the txout-set transaction count.
    pub(super) fn remaining_unspent(&self, txid: &TransactionHash) -> u32 {
        self.unspent_per_tx
            .get(txid)
            .map(|count| *count)
            .unwrap_or(0)
    }

    /// Test-only snapshot of the live unspent set, for asserting reconstruction.
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> std::collections::HashMap<Outpoint, TxOutCompact> {
        self.outputs
            .iter()
            .map(|entry| (*entry.key(), *entry.value()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::transparent_delta::TransparentBlockDelta;
    use super::*;
    use crate::chain_index::types::db::legacy::{ScriptType, TxLocation};

    /// A non-standard-but-spendable output (P2PK, bare multisig, …) compacts to
    /// `NonStandard`. zcashd lets it be spent, so `apply_forward` must cache it —
    /// otherwise `resolve_spent_outpoints_for_set_info` fails with "not in the UTXO
    /// cache" the first time a later block spends it.
    #[test]
    fn apply_forward_caches_nonstandard_spendable_output() {
        let cache = TransparentUtxoCache::new();
        let outpoint = Outpoint::new([7u8; 32], 0);
        let output = TxOutCompact::new(1_000, [9u8; 20], ScriptType::NonStandard as u8)
            .expect("a valid script-type byte builds a TxOutCompact");
        let delta = TransparentBlockDelta {
            created: vec![(outpoint, output, TxLocation::new(0, 0))],
            spent: vec![],
        };

        cache.apply_forward(&delta);

        assert_eq!(
            cache.value_of(&outpoint),
            Some(output),
            "a spendable NonStandard output must be resolvable for a later block's spend",
        );
    }
}
