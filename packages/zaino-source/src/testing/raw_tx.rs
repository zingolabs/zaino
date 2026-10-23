//! Real consensus-encoded transaction builders (v4 / v5, transparent only)

use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
use zcash_protocol::consensus::BranchId;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::{self as transparent, OutPoint, TxIn, TxOut};

type TransparentBundle = transparent::Bundle<transparent::Authorized>;

/// Real, empty v4 transaction (no inputs, outputs or bundles), distinct per `lock_time`
pub fn raw_transaction(lock_time: u32, expiry: u32) -> (TxId, Vec<u8>) {
    v4(lock_time, expiry, None)
}

/// Real v4 transaction spending `spends`, paying `pays` in vout order (no shielded bundles)
///
/// - empty script sigs (nothing here verifies a signature)
/// - distinct per `spends`
pub fn raw_transparent(spends: &[OutPoint], pays: &[(&Script, u64)]) -> (TxId, Vec<u8>) {
    let vin =
        spends.iter().map(|prevout| TxIn::from_parts(prevout.clone(), Script::default(), u32::MAX));
    let vout = pays.iter().map(|(script, zats)| {
        let value = Zatoshis::from_u64(*zats).expect("within the supply");
        TxOut::new(value, (*script).clone())
    });
    let bundle = TransparentBundle {
        vin: vin.collect(),
        vout: vout.collect(),
        authorization: transparent::Authorized,
    };
    v4(0, 0, Some(bundle))
}

/// Real, empty v5 transaction signed for `branch` (embedded in its bytes), distinct per
/// `lock_time`
pub fn raw_v5(branch: BranchId, lock_time: u32, expiry: u32) -> (TxId, Vec<u8>) {
    unshielded(TxVersion::V5, branch, lock_time, expiry, None)
}

fn v4(lock_time: u32, expiry: u32, transparent: Option<TransparentBundle>) -> (TxId, Vec<u8>) {
    unshielded(TxVersion::V4, BranchId::Canopy, lock_time, expiry, transparent)
}

fn unshielded(
    version: TxVersion,
    branch: BranchId,
    lock_time: u32,
    expiry: u32,
    transparent: Option<TransparentBundle>,
) -> (TxId, Vec<u8>) {
    let tx = TransactionData::<Authorized>::from_parts(
        version,
        branch,
        lock_time,
        expiry.into(),
        transparent,
        None,
        None,
        None,
    )
    .freeze()
    .expect("a transaction without shielded bundles freezes");
    let mut raw = Vec::new();
    tx.write(&mut raw).expect("writes to a Vec");
    (tx.txid(), raw)
}
