//! zebra-network's types ↔ Zaino's, at the wire boundary
//!
//! - every id handed out = recomputed by zebra from the bytes it parsed (a peer's claim never
//!   passes through)
//! - bytes out = zebra's re-serialization (byte-identical to the parsed encoding)

use std::sync::Arc;

use zaino_primitives::types::{BlockHash, TransactionId};
use zcash_primitives::transaction::{CompressedTransaction, DecompressionError};
use zebra_chain::block::{self, Block, CountedHeader};
use zebra_chain::serialization::{SerializationError, ZcashDeserialize, ZcashSerialize};
use zebra_chain::transaction::{self, AuthDigest, UnminedTx, UnminedTxId, WtxId};

/// Transaction as peers name it: txid + (v5+) auth digest (`MSG_WTX` inventory)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerTxId {
    pub txid: TransactionId,
    pub auth_digest: Option<[u8; 32]>,
}

/// Bytes zebra would not accept off the wire as one transaction
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("transaction bytes: {0}")]
    Parse(#[from] SerializationError),
    #[error("transaction points: {0}")]
    Points(#[from] DecompressionError),
    #[error("{0} trailing bytes after the transaction")]
    Trailing(usize),
}

pub(crate) fn peer_tx_id(id: UnminedTxId) -> PeerTxId {
    PeerTxId {
        txid: TransactionId::from(id.mined_id().0),
        auth_digest: id.auth_digest().map(|digest| digest.0),
    }
}

pub(crate) fn unmined_tx_id(id: PeerTxId) -> UnminedTxId {
    let mined = transaction::Hash(<[u8; 32]>::from(id.txid));
    match id.auth_digest {
        Some(digest) => UnminedTxId::from(WtxId { id: mined, auth_digest: AuthDigest(digest) }),
        None => UnminedTxId::from_legacy_id(mined),
    }
}

pub(crate) fn block_hash(hash: block::Hash) -> BlockHash {
    BlockHash::from(hash.0)
}

pub(crate) fn zebra_block_hash(hash: BlockHash) -> block::Hash {
    block::Hash(<[u8; 32]>::from(hash))
}

/// Consensus bytes of a header a peer sent
pub(crate) fn header_bytes(header: &CountedHeader) -> Vec<u8> {
    to_vec(header.header.as_ref())
}

/// Consensus bytes of a block a peer sent
pub(crate) fn block_bytes(block: &Block) -> Vec<u8> {
    to_vec(block)
}

/// Consensus bytes of a transaction a peer sent
pub(crate) fn transaction_bytes(tx: &UnminedTx) -> Vec<u8> {
    to_vec(tx.transaction.as_ref())
}

/// Raw bytes → zebra's unmined transaction, id computed from them (zebra's own parse-time
/// checks and point rules applied)
pub(crate) fn unmined_tx(raw: &[u8]) -> Result<UnminedTx, WireError> {
    let mut reader = raw;
    let parsed = CompressedTransaction::zcash_deserialize(&mut reader)?;
    if !reader.is_empty() {
        return Err(WireError::Trailing(reader.len()));
    }
    Ok(UnminedTx::try_from(Arc::new(parsed))?)
}

fn to_vec(value: &impl ZcashSerialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.zcash_serialize(&mut bytes).expect("writing to a Vec cannot fail");
    bytes
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Mainnet blocks (pre-v5 at 1, NU5 v5 at 1,687,107)
    ///
    /// - block + header bytes byte-identical
    /// - each tx re-parsed from its own bytes keeps its id (witnessed v5 id included)
    /// - trailing or truncated bytes refused
    #[test]
    fn real_blocks_and_their_transactions_round_trip_byte_identical() {
        let mut witnessed = 0;
        for vector in [
            zebra_test::vectors::BLOCK_MAINNET_1_BYTES.as_slice(),
            zebra_test::vectors::BLOCK_MAINNET_1687107_BYTES.as_slice(),
        ] {
            let block = Block::zcash_deserialize(vector).expect("mainnet vector");
            assert_eq!(block_bytes(&block), vector);
            let header = CountedHeader { header: Arc::clone(&block.header) };
            assert_eq!(header_bytes(&header), &vector[..header_bytes(&header).len()]);
            for tx in &block.transactions {
                let raw = to_vec(tx.as_ref());
                let unmined = unmined_tx(&raw).expect("a mined tx parses alone");
                assert_eq!(transaction_bytes(&unmined), raw);
                assert_eq!(unmined_tx_id(peer_tx_id(unmined.id)), unmined.id);
                witnessed += usize::from(unmined.id.auth_digest().is_some());

                let mut padded = raw.clone();
                padded.push(0);
                assert!(matches!(unmined_tx(&padded), Err(WireError::Trailing(1))));
                assert!(unmined_tx(&raw[..raw.len() - 1]).is_err(), "truncated");
            }
        }
        assert!(witnessed > 0, "a v5 transaction exercised the witnessed id");
    }

    proptest! {
        /// Either shape of id survives Zaino → zebra → Zaino unchanged (no byte order flipped)
        #[test]
        fn a_peer_tx_id_round_trips_through_zebras(
            txid in any::<[u8; 32]>(),
            auth_digest in proptest::option::of(any::<[u8; 32]>()),
        ) {
            let id = PeerTxId { txid: TransactionId::from(txid), auth_digest };
            prop_assert_eq!(peer_tx_id(unmined_tx_id(id)), id);
            prop_assert_eq!(unmined_tx_id(id).mined_id().0, txid);
        }
    }
}
