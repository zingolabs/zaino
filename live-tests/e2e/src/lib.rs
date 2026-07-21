//! Wallet-to-validator integration tests.

#![forbid(unsafe_code)]

use ztest::prelude::{CompactBlock, TxId};

/// A shielded/transparent pool, paired with the address kind that routes funds
/// into it. Lets a send-and-check test take a single `Pool` instead of an
/// address string plus a balance-field selector.
#[derive(Clone, Copy, Debug)]
pub enum Pool {
    /// Orchard (funds routed via a unified address through NU6.2; from NU6.3
    /// devtool routes unified-address outputs to Ironwood — use
    /// [`Pool::Ironwood`] for the receipt pool on NU6.3-active chains).
    Orchard,
    /// Ironwood (funds routed via a unified address from NU6.3).
    Ironwood,
    /// Sapling.
    Sapling,
    /// Transparent.
    Transparent,
}

impl Pool {
    /// The pool name that routes funds into this pool.
    pub fn address_kind(self) -> &'static str {
        match self {
            Pool::Orchard | Pool::Ironwood => "unified",
            Pool::Sapling => "sapling",
            Pool::Transparent => "transparent",
        }
    }

    pub fn ztest(self) -> ztest::Pool {
        match self {
            Pool::Orchard => ztest::Pool::Orchard,
            Pool::Ironwood => ztest::Pool::Ironwood,
            Pool::Sapling => ztest::Pool::Sapling,
            Pool::Transparent => ztest::Pool::Transparent,
        }
    }

    pub fn spendable_balance(self, balances: &ztest::PoolBalances) -> u64 {
        balances.get(self.ztest())
    }
}

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
