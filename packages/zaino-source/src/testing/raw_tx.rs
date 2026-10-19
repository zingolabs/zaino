//! Real consensus-encoded transaction builders (v4 / v5, transparent only) + their decode

use zaino_primitives::types::Transaction;
use zcash_protocol::TxId;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::OutPoint;

/// Real, empty v4 transaction (no inputs, outputs or bundles), distinct per `lock_time`
pub fn raw_transaction(lock_time: u32, expiry: u32) -> (TxId, Vec<u8>) {
    v4(lock_time, expiry, None)
}

/// Real v4 transaction spending `spends`, paying `pays` in vout order (no shielded bundles)
///
/// - empty script sigs (nothing here verifies a signature)
/// - distinct per `spends`
pub fn raw_transparent(spends: &[OutPoint], pays: &[(&Script, u64)]) -> (TxId, Vec<u8>) {
    use zcash_transparent::bundle::{Authorized, Bundle, TxIn, TxOut};
    let vin =
        spends.iter().map(|prevout| TxIn::from_parts(prevout.clone(), Script::default(), u32::MAX));
    let vout = pays.iter().map(|(script, zats)| {
        let value = zcash_protocol::value::Zatoshis::from_u64(*zats).expect("within the supply");
        TxOut::new(value, (*script).clone())
    });
    let bundle = Bundle { vin: vin.collect(), vout: vout.collect(), authorization: Authorized };
    v4(0, 0, Some(bundle))
}

/// Real, empty v5 transaction signed for `branch` (embedded in its bytes), distinct per
/// `lock_time`
pub fn raw_v5(
    branch: zcash_protocol::consensus::BranchId,
    lock_time: u32,
    expiry: u32,
) -> (TxId, Vec<u8>) {
    use zcash_primitives::transaction::TxVersion;
    unshielded(TxVersion::V5, branch, lock_time, expiry, None)
}

fn v4(
    lock_time: u32,
    expiry: u32,
    transparent: Option<zcash_transparent::bundle::Bundle<zcash_transparent::bundle::Authorized>>,
) -> (TxId, Vec<u8>) {
    use zcash_primitives::transaction::TxVersion;
    let canopy = zcash_protocol::consensus::BranchId::Canopy;
    unshielded(TxVersion::V4, canopy, lock_time, expiry, transparent)
}

fn unshielded(
    version: zcash_primitives::transaction::TxVersion,
    branch: zcash_protocol::consensus::BranchId,
    lock_time: u32,
    expiry: u32,
    transparent: Option<zcash_transparent::bundle::Bundle<zcash_transparent::bundle::Authorized>>,
) -> (TxId, Vec<u8>) {
    use zcash_primitives::transaction::{Authorized, TransactionData};
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

/// `raw` decoded beside its bytes (`BlockBuilder::raw_tx(decoded(raw))`)
pub fn decoded(raw: Vec<u8>) -> (Transaction, Vec<u8>) {
    let tx = crate::decode_transaction(&raw).expect("decoded takes real transaction bytes");
    (tx, raw)
}
