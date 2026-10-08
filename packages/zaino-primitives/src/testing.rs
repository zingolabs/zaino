//! Regtest chains with real header bytes: hash = SHA-256d of the header, `prev_hash` links, merkle
//! root = the txids' (`cfg(test)` too: a crate's own features don't self-enable)

mod build;
mod mock_chain;
mod upgrades;

use crate::sha256d;

pub use build::{outpoint, p2pkh, BlockBuilder, TxBuilder};
pub use mock_chain::{Branch, MockChain, Schedule, Work};
pub use upgrades::Upgrades;

use crate::types::{BlockHash, BlockHeader, EquihashSolution, Height, Transaction};

/// `Height` from a literal (`chain.fork(h(9))`)
pub fn h(height: u32) -> Height {
    Height::try_from(height).expect("a test height below 2^31")
}

/// Transparent value `tx` leaves the miner: `spent` (its inputs' sum) − outputs + each pool's
/// balance (protocol.pdf#transactions §3.4; negative = overspent)
pub fn fee_left(tx: &Transaction, spent: i64) -> i64 {
    let paid: i64 = tx.transparent.outputs.iter().map(|output| output.value.as_i64()).sum();
    spent - paid + balances(tx).iter().sum::<i64>()
}

/// Sprout, Sapling, Orchard, Ironwood (+ = out of that pool)
fn balances(tx: &Transaction) -> [i64; 4] {
    [
        tx.sprout.value_balance,
        tx.sapling.value_balance,
        tx.orchard.value_balance,
        tx.ironwood.value_balance,
    ]
    .map(i64::from)
}

/// zcashd regtest `powLimit` as nBits (the only nBits regtest accepts)
const REGTEST_BITS: u32 = 0x200f_0f0f;
const GENESIS_TIME: u32 = 1_700_000_000;

/// Consensus bytes of `header` (`hash` + `height` derived from them, never encoded)
pub fn encode_header(header: &BlockHeader) -> Vec<u8> {
    let solution = header.solution.as_bytes();
    let mut raw = Vec::with_capacity(143 + solution.len());
    raw.extend(header.version.to_le_bytes());
    raw.extend(<[u8; 32]>::from(header.prev_hash));
    raw.extend(<[u8; 32]>::from(header.merkle_root));
    raw.extend(<[u8; 32]>::from(header.block_commitments));
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
pub fn header_hash(header: &BlockHeader) -> BlockHash {
    BlockHash::from(sha256d(&encode_header(header)))
}
