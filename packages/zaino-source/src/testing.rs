//! Test chains of real consensus bytes, and validators serving them
//!
//! - [`MockChain`]: a regtest block tree, every transaction minted by librustzcash (`mint.rs`)
//! - Every block handed out = [`decode_block`](crate::decode_block) of its bytes
//! - [`MockValidator`]: one simulated zebrad over a `MockChain`'s blocks

mod build;
mod chain;
pub mod fixtures;
mod ledger;
mod mint;
mod upgrades;
mod validator;

use std::time::Duration;

use zaino_primitives::sha256d;
use zaino_primitives::types::{DecodedHeader, EquihashSolution, Transaction};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, TxId};
use zcash_transparent::address::Script;
use zcash_transparent::bundle::OutPoint;

pub use build::{p2pkh, BlockBuilder, TxBuilder};
pub use chain::{Branch, MockChain, Schedule, Work};
pub use ledger::{Ledger, Refused, Unmined};
pub use mint::{commitment, nullifier};
pub use upgrades::all_at;
pub use validator::{Lie, MockValidator};

/// `BlockHeight` from a literal (`chain.fork(h(9))`)
pub fn h(height: u32) -> BlockHeight {
    BlockHeight::from(height)
}

/// zcashd regtest `powLimit` as nBits (the only nBits regtest accepts)
const REGTEST_BITS: u32 = 0x200f_0f0f;
const GENESIS_TIME: u32 = 1_700_000_000;

/// Consensus bytes of `header` (`hash` + `height` derived from them, never encoded)
pub fn encode_header(header: &DecodedHeader) -> Vec<u8> {
    let solution = header.solution.as_bytes();
    let mut raw = Vec::with_capacity(143 + solution.len());
    raw.extend(header.version.to_le_bytes());
    raw.extend(header.prev_hash.0);
    raw.extend(header.merkle_root.0);
    raw.extend(header.block_commitments.0);
    raw.extend(header.time.to_le_bytes());
    raw.extend(header.bits.bits().to_le_bytes());
    raw.extend(header.nonce);
    match header.solution {
        EquihashSolution::Standard(_) => raw.extend([0xfd, 0x40, 0x05]),
        EquihashSolution::Regtest(_) => raw.push(36),
    }
    raw.extend(solution);
    raw
}

/// What `header.hash` must be: SHA-256d of [`encode_header`]
pub fn header_hash(header: &DecodedHeader) -> BlockHash {
    BlockHash(sha256d(&encode_header(header)))
}

/// `waited` within 60 s of tokio time, else the test fails naming `what`
///
/// - paused clock: 60 s virtual = instant, so a wait nothing will end (a broken driver, a fold
///   never publishing) fails at once instead of hanging the runner
pub async fn within<T>(what: &str, waited: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(60), waited).await {
        Ok(done) => done,
        Err(_) => panic!("never {what} (60 s)"),
    }
}

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

/// `raw` decoded beside its bytes
pub fn decoded(raw: Vec<u8>) -> (Transaction, Vec<u8>) {
    let tx = crate::decode_transaction(&raw).expect("decoded takes real transaction bytes");
    (tx, raw)
}
