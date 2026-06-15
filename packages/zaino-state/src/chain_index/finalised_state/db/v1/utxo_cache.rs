//! In-memory transparent UTXO cache for the txout-set accumulator.
//!
//! **SKELETON** — design under review, not yet wired into the accumulator. Its
//! items are `#[allow(dead_code)]` until the staged wiring below lands.
//!
//! # Why
//!
//! `calculate_tx_out_set_info_accumulator_after_block` resolves, per spent
//! transparent input, three things by reading the database:
//!
//! - the spent output's value     → `transparent` table (a cold historical read)
//! - the spent output's identity  → `txid_location` table (a random read)
//! - a prior tx's remaining-unspent count → `spent` table (a random read on
//!   the `unspent_output_counts` cache miss)
//!
//! Once those indexes exceed RAM these become random B+tree page faults on the
//! write-hot path — the dominant cost of the sync slowdown (see the
//! `project_sync_write_path_read_audit` memory). But a *forward* sync observes
//! every transparent output, value included (`TxOutCompact` carries `value`), at
//! the moment it ingests the creating block. Every output is created before it
//! is spent, so all three answers can be served from carried-forward memory
//! instead of faulting back to disk.
//!
//! # What this holds
//!
//! The live *unspent* transparent UTXO set:
//!
//! - `outputs`:        outpoint → its unspent output (the value).  [value resolution]
//! - `unspent_per_tx`: txid     → count of its still-unspent outputs.  [tx count]
//!
//! `unspent_per_tx` **subsumes [`super::DbV1`]'s `unspent_output_counts`**: that
//! field's role — the per-tx unspent count, today a cache that falls back to a
//! `spent` read on a miss — moves here and becomes authoritative, leaving one
//! owning cache instead of two overlapping ones.
//!
//! # Constraint honored
//!
//! No schema change. This is purely in-memory *derived* state, reconstructable
//! from the committed `transparent` and `spent` tables; nothing new is persisted.
//!
//! # Lifecycle (staged wiring — each stage compiles and is independently verifiable)
//!
//! 1. **Seed on open.** `DbV1` scans committed `transparent` minus `spent` and
//!    calls [`TransparentUtxoCache::record_created`] for each unspent output. A
//!    no-op for a from-genesis sync; a one-time scan on resume, before the first
//!    new block. (Reconstruct-on-open is what keeps this schema-free.)
//! 2. **Maintain on commit.** The block-write path calls `record_created` /
//!    `record_spent` as it commits. Run alongside the existing accumulator and
//!    assert the served values match before relying on it.
//! 3. **Flip the reads.** Point the accumulator at [`value_of`] /
//!    [`remaining_unspent`], then delete `load_prior_transactions`, the
//!    `get_outpoint_spenders` unspent-count read, and `DbV1::unspent_output_counts`.
//! 4. **Reorg.** `delete_block` inverts the maintenance: re-insert the block's
//!    spent outputs, remove the ones it created.
//!
//! [`value_of`]: TransparentUtxoCache::value_of
//! [`remaining_unspent`]: TransparentUtxoCache::remaining_unspent

#![allow(dead_code)] // skeleton: wired in stages (see module docs)

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
            if let Some(mut count) = self.unspent_per_tx.get_mut(&txid) {
                *count = count.saturating_sub(1);
            }
            // count == 0 entries are pruned during stage-3 wiring.
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
