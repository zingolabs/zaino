//! Wallet-to-validator integration tests.

#![forbid(unsafe_code)]

use ztest::prelude::{CompactBlock, TxId};

/// The harness value-pool selector. Re-exported from ztest so the tests, the
/// wallet API (`send`/`send_from`), and the compact-block assertions below all
/// speak one `Pool` type — no conversion at the call site.
pub use ztest::Pool;

/// Whether the compact tx with `txid` carries no data for `pool` (transparent
/// `vout` / sapling `outputs` / orchard `actions` / `ironwood_actions`).
fn pool_tx_field_empty(block: &CompactBlock, txid: &TxId, pool: Pool) -> bool {
    let tx = block
        .vtx
        .iter()
        .find(|tx| tx.txid == txid.as_ref().to_vec())
        .expect("sent tx present in compact block");
    match pool {
        Pool::Transparent => tx.vout.is_empty(),
        Pool::Sapling => tx.outputs.is_empty(),
        Pool::Orchard => tx.actions.is_empty(),
        Pool::Ironwood => tx.ironwood_actions.is_empty(),
    }
}

/// Assert the compact tx with `txid` carries `pool` data.
pub fn assert_pool_present(block: &CompactBlock, txid: &TxId, pool: Pool) {
    assert!(
        !pool_tx_field_empty(block, txid, pool),
        "{pool:?} data should be present in the compact block"
    );
}

/// Assert the compact tx with `txid` carries no `pool` data.
pub fn assert_pool_absent(block: &CompactBlock, txid: &TxId, pool: Pool) {
    assert!(
        pool_tx_field_empty(block, txid, pool),
        "{pool:?} data should be absent from the compact block"
    );
}
