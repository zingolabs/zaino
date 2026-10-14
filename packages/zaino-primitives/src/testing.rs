//! Regtest chains with real header bytes: hash = SHA-256d of the header, `prev_hash` links, merkle
//! root = the txids' (`cfg(test)` too: a crate's own features don't self-enable)

mod build;
mod ledger;
mod mock_chain;
mod upgrades;

use crate::sha256d;

pub use build::{p2pkh, BlockBuilder, TxBuilder};
pub use ledger::{Ledger, Refused, Unmined};
pub use mock_chain::{Branch, MockChain, Schedule, Work};
pub use upgrades::all_at;

use crate::types::{DecodedHeader, EquihashSolution};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

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
pub fn header_hash(header: &DecodedHeader) -> BlockHash {
    BlockHash(sha256d(&encode_header(header)))
}
