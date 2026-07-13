//! Wallet-to-validator integration tests.
//!
//! These exercise the zcash-devtool wallet client (a faucet and a recipient,
//! see [`devtool`]) against a running Zaino indexer, built from a launched
//! [`zaino_testutils::TestManager`]'s gRPC address. The clients are managed by
//! `zcash_local_net`; this workspace keeps its own Cargo.lock for that stack.

#![forbid(unsafe_code)]

use zaino_proto::proto::compact_formats::CompactBlock;
use zcash_primitives::transaction::TxId;

pub mod devtool;

/// A shielded/transparent pool, paired with the address kind that routes funds
/// into it. Lets a send-and-check test take a single `Pool` instead of an
/// address string plus a balance-field closure.
#[derive(Clone, Copy, Debug)]
pub enum Pool {
    /// Orchard (funds routed via a unified address).
    Orchard,
    /// Sapling.
    Sapling,
    /// Transparent.
    Transparent,
}

impl Pool {
    /// The `get_recipient_address` / `get_faucet_address` pool name that routes
    /// funds into this pool.
    pub fn address_kind(self) -> &'static str {
        match self {
            Pool::Orchard => "unified",
            Pool::Sapling => "sapling",
            Pool::Transparent => "transparent",
        }
    }
}

/// Whether the compact tx with `txid` carries no data for `pool` (transparent
/// `vout` / sapling `outputs` / orchard `actions`).
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
