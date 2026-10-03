//! Consensus-encoded block bytes, projected straight to the domain [`Block`].
//!
//! A validator's raw block read (`getblock <h> 0`, or the bytes a state
//! database holds) carries far more than an indexer reads: proofs, signatures,
//! value commitments, full note ciphertexts, input scripts. A full
//! deserialiser pays for all of it, and on shielded-heavy blocks most of that
//! cost is curve arithmetic — decompressing every value commitment and
//! ephemeral key — for fields the indexes then discard.
//!
//! This crate walks the encoding instead. It reads the fields the domain block
//! keeps — transparent outpoints and outputs, shielded nullifiers, note
//! commitments, ephemeral keys and the compact ciphertext head, value balances
//! — as the bytes they are on the wire, and steps over everything else by
//! size. No curve point is decompressed and no field element is reduced. The
//! transaction id is computed over the same bytes: the double-SHA256 of the
//! encoding for v1–v4, the ZIP-244 digest tree for v5 and v6.
//!
//! The result is the same [`Block`] the zebra-backed conversion produces, and
//! the crate's tests hold it to that: every fixture block is decoded both ways
//! and compared field for field.
//!
//! ```text
//! decode(bytes) = project ∘ walk                          walk never touches a curve
//! decode(bytes) == block_from_zebra(deserialise(bytes))   (the oracle)
//! ```
#![forbid(unsafe_code)]

mod error;
mod header;
mod project;
mod reader;
mod transaction;
mod txid;

use zaino_primitives::types::{Block, ChainMetadata, Transaction};

pub use error::DecodeError;

use header::{coinbase_height, project_header, read_header};
use project::project_transaction;
use reader::Reader;
use transaction::{RawTransaction, read_transaction};

/// The fewest bytes a transaction can occupy: a version word, empty input and
/// output vectors, and a lock time. Bounds the transaction count a block may
/// claim before any of them is read.
const MIN_TRANSACTION_SIZE: usize = 4 + 1 + 1 + 4;

/// Decode one block's consensus encoding.
///
/// `chain_metadata` is supplied by the caller for the same reason the zebra
/// conversion takes it: cumulative tree sizes are indexed state, not present in
/// the block bytes, so the decoder cannot know them.
///
/// Rejects trailing bytes, a block without transactions, a first transaction
/// that is not a coinbase, and a coinbase whose height push is not canonical —
/// the same shape checks zebra's deserialiser makes — but does not validate
/// the block: proofs, signatures and curve points are never examined.
pub fn decode_block(raw: &[u8], chain_metadata: ChainMetadata) -> Result<Block, DecodeError> {
    let mut reader = Reader::new(raw);
    let header = read_header(&mut reader)?;
    let transactions = reader.vector(MIN_TRANSACTION_SIZE, read_transaction)?;
    reject_trailing(&reader)?;

    let coinbase = transactions.first().ok_or(DecodeError::NoTransactions)?;
    let coinbase_input = match coinbase.inputs.as_slice() {
        [only] if only.is_coinbase() => only,
        _ => return Err(DecodeError::NoCoinbase),
    };
    let height = coinbase_height(&header.prev_hash, coinbase_input.script_sig)?;

    let header = project_header(&header, height)?;
    let transactions = transactions
        .iter()
        .map(project_transaction)
        .collect::<Result<Vec<Transaction>, DecodeError>>()?;
    Ok(Block::try_new(header, transactions, chain_metadata)?)
}

/// Decode one transaction's consensus encoding on its own — a mempool
/// transaction, say, which has no block around it.
pub fn decode_transaction(raw: &[u8]) -> Result<Transaction, DecodeError> {
    let mut reader = Reader::new(raw);
    let transaction: RawTransaction<'_> = read_transaction(&mut reader)?;
    reject_trailing(&reader)?;
    project_transaction(&transaction)
}

fn reject_trailing(reader: &Reader<'_>) -> Result<(), DecodeError> {
    match reader.remaining() {
        0 => Ok(()),
        trailing => Err(DecodeError::Trailing(trailing)),
    }
}
