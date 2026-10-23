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
mod raw_tx;
mod upgrades;
mod validator;

use std::time::Duration;

use zaino_primitives::sha256d;
use zaino_primitives::types::{DecodedHeader, EquihashSolution};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

pub use build::{p2pkh, BlockBuilder, TxBuilder};
pub use chain::{Branch, MockChain, Schedule, Work};
pub use ledger::{Ledger, Refused, Unmined};
pub use mint::{commitment, nullifier};
pub use raw_tx::{raw_transaction, raw_transparent, raw_v5};
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
fn header_hash(header: &DecodedHeader) -> BlockHash {
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
