//! Proof-of-work quantities for chain selection.
//!
//! Work is one unit, but three different quantities wear it. The expected
//! work of a single block — derived from its difficulty target — the
//! cumulative work at a block — the fold of block works along its chain — and
//! the work a branch accumulates since an anchor are related but not
//! interchangeable: adding two cumulative values is meaningless (no chain is
//! the concatenation of two chains), a single block's work is not a
//! chain-selection candidate, and a branch's relative work admits zero where
//! cumulative work cannot. So each quantity is its own type:
//!
//! - [`BlockWork`] — the expected work of one block. Strictly positive.
//! - [`ChainWork`] — cumulative work at a block. Strictly positive, and
//!   ordered: comparing cumulative work is chain selection.
//! - [`RelativeWork`] — work accumulated since an anchor: what a branch adds
//!   on top of the absolute cumulative work at the block it forks from. Zero
//!   is admissible — a branch whose tip is the anchor has accumulated
//!   nothing.
//!
//! Folding block works into cumulative work — seeding at genesis, accumulating
//! forward, rolling back on reorg — is a set of relations between the two
//! types; they live in the [`arithmetic`] module alongside the algebra that
//! governs them. See ADR-0013 for the doctrine.
//!
//! Deriving a [`BlockWork`] from a difficulty target lives on
//! [`CompactDifficulty`](super::CompactDifficulty), whose
//! [`to_work`](super::CompactDifficulty::to_work) runs the native
//! nBits → target → work pipeline and lands here. [`BlockWork::try_new`]
//! remains the door for a work integer computed elsewhere.

mod arithmetic;
mod block_work;
mod chain_work;
mod relative_work;

pub use arithmetic::{WorkOverflow, WorkUnderflow};
pub use block_work::{BlockWork, ZeroWork};
pub use chain_work::{ChainWork, ChainWorkOverWidth};
pub use relative_work::RelativeWork;
