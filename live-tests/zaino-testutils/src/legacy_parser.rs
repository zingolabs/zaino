//! An independent Zcash block and transaction parser, for test vectors only.
//!
//! This was `zaino-fetch`'s `chain` module. It is here, in the test tree,
//! because that is the only place it still has a job.
//!
//! # Why keep a second parser at all
//!
//! Production parses blocks through `zebra-chain`. The live-test vectors
//! (`e2e/tests/test_vectors.rs`, `clientless/tests/test_vectors.rs`) exist to
//! catch the class of bug where a parser is subtly wrong — a field read at the
//! wrong offset, a byte order flipped — and they catch it by parsing the same
//! bytes a second, independent way and comparing. Rewriting these vectors
//! against `zebra-chain` would collapse the oracle onto the parser under test
//! and remove the reason the vectors are there.
//!
//! So it stays, and it stays *here*: the cost is a few hundred lines living in
//! the test tree, and in exchange no production crate carries a parser it does
//! not use.
//!
//! # Not a maintained implementation
//!
//! Nothing outside the live tests may depend on this. It is not kept in step
//! with consensus changes beyond what the vectors need, and a disagreement
//! between this and `zebra-chain` is a finding to investigate, not a bug to fix
//! by editing this side until they match.

pub mod block;
pub mod error;
pub mod transaction;
pub mod utils;

mod indexed_block;

pub use indexed_block::{compact_tx_data_from_full_transaction, indexed_block_from_full_block};
