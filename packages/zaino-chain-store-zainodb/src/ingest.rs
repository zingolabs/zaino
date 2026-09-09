//! - `block_fetch` + `treestate_fetch` + `block_assemble` = the per-block cost; all
//!   three scope-bound, so work cannot move between them or be recovered by subtraction

#[cfg(not(feature = "transparent_address_history_experimental"))]
use crate::types::IndexedBlock;

/// Protocol work in one block, from a single walk feeding both
/// [`approx_bytes`](Self::approx_bytes) and [`record`](Self::record)
// Gated with its only consumer: the experimental feature compiles the batch path out
#[cfg(not(feature = "transparent_address_history_experimental"))]
#[derive(Clone, Copy, Default)]
pub(crate) struct BlockWork {
    transactions: u64,
    transparent_inputs: u64,
    transparent_outputs: u64,
    sapling_spends: u64,
    sapling_outputs: u64,
    orchard_actions: u64,
    ironwood_actions: u64,
}

#[cfg(not(feature = "transparent_address_history_experimental"))]
impl BlockWork {
    /// - No Sprout JoinSplits (absent from the stored compact tx model)
    /// - Saturating: a corrupt source read must degrade a metric, not panic ingest
    pub(crate) fn tally(block: &IndexedBlock) -> Self {
        let mut work = Self {
            transactions: block.transactions().len() as u64,
            ..Self::default()
        };
        for tx in block.transactions() {
            let transparent = tx.transparent();
            for (field, count) in [
                (&mut work.transparent_inputs, transparent.inputs().len()),
                (&mut work.transparent_outputs, transparent.outputs().len()),
                (&mut work.sapling_spends, tx.sapling().spends().len()),
                (&mut work.sapling_outputs, tx.sapling().outputs().len()),
                (&mut work.orchard_actions, tx.orchard().actions().len()),
                (&mut work.ironwood_actions, tx.ironwood().actions().len()),
            ] {
                *field = field.saturating_add(count as u64);
            }
        }
        work
    }

    fn operations(self) -> u64 {
        self.transparent_inputs
            .saturating_add(self.transparent_outputs)
            .saturating_add(self.sapling_spends)
            .saturating_add(self.sapling_outputs)
            .saturating_add(self.orchard_actions)
            .saturating_add(self.ironwood_actions)
    }

    /// Rough heap size of a buffered [`IndexedBlock`]; bounds the bulk-sync batch
    pub(crate) fn approx_bytes(self) -> u64 {
        self.transactions
            .saturating_mul(256)
            .saturating_add(self.operations().saturating_mul(128))
    }

    pub(crate) fn record(self) {
        let counters = block_counters();
        for (counter, count) in [
            (&counters.transactions, self.transactions),
            (&counters.transparent_inputs, self.transparent_inputs),
            (&counters.transparent_outputs, self.transparent_outputs),
            (&counters.sapling_spends, self.sapling_spends),
            (&counters.sapling_outputs, self.sapling_outputs),
            (&counters.orchard_actions, self.orchard_actions),
            (&counters.ironwood_actions, self.ironwood_actions),
        ] {
            counter.increment(count);
        }
    }
}

/// - `counter!()` builds a `Key`, hashes it and read-locks a registry shard *per call*;
///   a cached handle leaves only the `fetch_add`, and sync makes millions of calls
/// - Gated with `BlockWork`: the experimental feature compiles the batch path out, and
///   an ungated cache there is 7 handles nothing can ever increment
#[cfg(not(feature = "transparent_address_history_experimental"))]
struct BlockCounters {
    transactions: metrics::Counter,
    transparent_inputs: metrics::Counter,
    transparent_outputs: metrics::Counter,
    sapling_spends: metrics::Counter,
    sapling_outputs: metrics::Counter,
    orchard_actions: metrics::Counter,
    ironwood_actions: metrics::Counter,
}

/// - Steady-state read is an acquire load, no lock
/// - Resolves against whatever recorder is live at the first block. Seeding does NOT
///   go through here: a cached handle would bind the seed to one recorder for the
///   life of the process
#[cfg(not(feature = "transparent_address_history_experimental"))]
fn block_counters() -> &'static BlockCounters {
    static COUNTERS: std::sync::OnceLock<BlockCounters> = std::sync::OnceLock::new();
    COUNTERS.get_or_init(|| {
        use crate::metric_names::*;
        BlockCounters {
            transactions: metrics::counter!(SYNC_TRANSACTIONS_TOTAL),
            transparent_inputs: metrics::counter!(SYNC_TRANSPARENT_INPUTS_TOTAL),
            transparent_outputs: metrics::counter!(SYNC_TRANSPARENT_OUTPUTS_TOTAL),
            sapling_spends: metrics::counter!(SYNC_SAPLING_SPENDS_TOTAL),
            sapling_outputs: metrics::counter!(SYNC_SAPLING_OUTPUTS_TOTAL),
            orchard_actions: metrics::counter!(SYNC_ORCHARD_ACTIONS_TOTAL),
            ironwood_actions: metrics::counter!(SYNC_IRONWOOD_ACTIONS_TOTAL),
        }
    })
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    use super::*;

    /// - Oracle = the zebra blocks, not the compact model (re-walking `IndexedBlock`
    ///   restates `tally`; the source also catches a lossy convert)
    #[test]
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    fn tally_matches_the_zebra_blocks_the_index_was_built_from() {
        let blocks = crate::tests::vectors::load_vector_blocks()
            .expect("regtest test vectors are checked in beside this crate");
        let mut asserted_shielded = false;

        for (vector, indexed) in blocks
            .iter()
            .zip(crate::tests::fixtures::indexed_block_chain(&blocks))
        {
            let work = BlockWork::tally(&indexed);
            let txs = &vector.zebra_block.transactions;
            let expect = |f: fn(&zebra_chain::transaction::Transaction) -> usize| -> u64 {
                txs.iter().map(|tx| f(tx) as u64).sum()
            };

            // Per class, not on a total: two swapped leaves any total intact, and a
            // new pool without a row here is a visible gap
            let tallied = [
                work.transactions,
                work.transparent_inputs,
                work.transparent_outputs,
                work.sapling_spends,
                work.sapling_outputs,
                work.orchard_actions,
                work.ironwood_actions,
            ];
            let expected = [
                txs.len() as u64,
                expect(|tx| tx.inputs().len()),
                expect(|tx| tx.outputs().len()),
                expect(|tx| tx.sapling_spends_per_anchor().count()),
                expect(|tx| tx.sapling_outputs().count()),
                expect(|tx| tx.orchard_actions().count()),
                expect(|tx| tx.ironwood_actions().count()),
            ];
            assert_eq!(
                tallied, expected,
                "tally disagrees with the zebra block at height {} (order: txs, \
                 t-in, t-out, s-spend, s-out, orchard, ironwood)",
                vector.height
            );

            asserted_shielded |=
                work.sapling_outputs > 0 || work.orchard_actions > 0 || work.ironwood_actions > 0;
        }

        // Shielded-free vectors pass every assert above while proving nothing
        assert!(
            asserted_shielded,
            "the test vectors carry no shielded output or action"
        );
    }
}
