//! NFS metrics (names = ztest's `zainod` families: a rename breaks its sync probes)

use zaino_primitives::types::{Block, Height, Transaction};

mod names {
    pub(super) const BEST_TIP: &str = "zaino.best_tip";
    pub(super) const REORGS_TOTAL: &str = "zaino.reorgs_total";
    pub(super) const FETCH_HEIGHT: &str = "zaino.fetch_height";
    pub(super) const FETCH_BLOCKS_TOTAL: &str = "zaino.fetch.blocks_total";
    pub(super) const FETCH_TRANSACTIONS_TOTAL: &str = "zaino.fetch.transactions_total";
    pub(super) const FETCH_TRANSPARENT_INPUTS_TOTAL: &str = "zaino.fetch.transparent_inputs_total";
    pub(super) const FETCH_TRANSPARENT_OUTPUTS_TOTAL: &str =
        "zaino.fetch.transparent_outputs_total";
    pub(super) const FETCH_SAPLING_SPENDS_TOTAL: &str = "zaino.fetch.sapling_spends_total";
    pub(super) const FETCH_SAPLING_OUTPUTS_TOTAL: &str = "zaino.fetch.sapling_outputs_total";
    pub(super) const FETCH_ORCHARD_ACTIONS_TOTAL: &str = "zaino.fetch.orchard_actions_total";
    pub(super) const FETCH_IRONWOOD_ACTIONS_TOTAL: &str = "zaino.fetch.ironwood_actions_total";
}

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge};

    describe_gauge!(names::BEST_TIP, "Verified tip height the NFS follows");
    describe_counter!(
        names::REORGS_TOTAL,
        "Served tips replaced by another branch rather than extended"
    );
    // published from boot (unregistered until first reorg = indistinguishable from unemitted)
    metrics::counter!(names::REORGS_TOTAL).absolute(0);
    describe_gauge!(
        names::FETCH_HEIGHT,
        "Highest height handed to the indexes (folded, or sent final unfolded); rewinds on a reorg"
    );
    for (name, what) in [
        (names::FETCH_BLOCKS_TOTAL, "Blocks"),
        (names::FETCH_TRANSACTIONS_TOTAL, "Transactions"),
        (names::FETCH_TRANSPARENT_INPUTS_TOTAL, "Transparent inputs"),
        (names::FETCH_TRANSPARENT_OUTPUTS_TOTAL, "Transparent outputs"),
        (names::FETCH_SAPLING_SPENDS_TOTAL, "Sapling spends"),
        (names::FETCH_SAPLING_OUTPUTS_TOTAL, "Sapling outputs"),
        (names::FETCH_ORCHARD_ACTIONS_TOTAL, "Orchard actions"),
        (names::FETCH_IRONWOOD_ACTIONS_TOTAL, "Ironwood actions"),
    ] {
        describe_counter!(
            name,
            format!("{what} handed to the indexes; a reorg's branch or a restart counts again")
        );
    }
}

pub(crate) fn best(height: Height) {
    metrics::gauge!(names::BEST_TIP).set(f64::from(u32::from(height)));
}

pub(crate) fn reorg() {
    metrics::counter!(names::REORGS_TOTAL).increment(1);
}

/// `block` handed to the indexes (folded, or sent unfolded)
pub(crate) fn handed(block: &Block) {
    let txs = block.transactions();
    let sum = |count: fn(&Transaction) -> usize| txs.iter().map(count).sum::<usize>() as u64;

    metrics::gauge!(names::FETCH_HEIGHT).set(f64::from(u32::from(block.header().height)));
    metrics::counter!(names::FETCH_BLOCKS_TOTAL).increment(1);
    metrics::counter!(names::FETCH_TRANSACTIONS_TOTAL).increment(txs.len() as u64);
    metrics::counter!(names::FETCH_TRANSPARENT_INPUTS_TOTAL)
        .increment(sum(|tx| tx.transparent.inputs.len()));
    metrics::counter!(names::FETCH_TRANSPARENT_OUTPUTS_TOTAL)
        .increment(sum(|tx| tx.transparent.outputs.len()));
    metrics::counter!(names::FETCH_SAPLING_SPENDS_TOTAL)
        .increment(sum(|tx| tx.sapling.spends.len()));
    metrics::counter!(names::FETCH_SAPLING_OUTPUTS_TOTAL)
        .increment(sum(|tx| tx.sapling.outputs.len()));
    metrics::counter!(names::FETCH_ORCHARD_ACTIONS_TOTAL)
        .increment(sum(|tx| tx.orchard.actions.len()));
    metrics::counter!(names::FETCH_IRONWOOD_ACTIONS_TOTAL)
        .increment(sum(|tx| tx.ironwood.actions.len()));
}
