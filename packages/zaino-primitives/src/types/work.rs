//! Proof-of-work quantities.
//!
//! Work is one unit, but three quantities are measured in it:
//!
//! - [`SingleBlockWork`] — the work one block is expected to take, derived
//!   from its difficulty target.
//! - [`AbsoluteChainWork`] — the total work of a chain up to and including a
//!   block. This is the value validators report as `chainwork`.
//! - [`RelativeChainWork`] — the work a run of consecutive blocks holds,
//!   measured from wherever the run begins.
//!
//! They are separate types because merging them would let wrong statements
//! compile.
//!
//! **Ordering.** Comparing total chain work is how the best chain is chosen, so
//! [`AbsoluteChainWork`] is `Ord`. [`SingleBlockWork`] is not. Difficulty is
//! fixed within a retarget interval, so two competing blocks at the same height
//! have equal work. An ordering on single blocks would report a tie for exactly
//! the case chain selection exists to resolve.
//!
//! **Seeding.** A chain of one block has total work equal to that block's work,
//! but the two remain different quantities. With one type the seed and the
//! summand have the same signature, so passing the wrong one at the start of a
//! fold compiles and corrupts every value after it. With two types, only
//! [`AbsoluteChainWork::genesis`] crosses between them.
//!
//! **Origin.** [`AbsoluteChainWork`] counts from genesis;
//! [`RelativeChainWork`] counts from wherever its run begins, so it is a total
//! over a set of blocks rather than a value at one block. The two are the same
//! integer measured from different places, and the difference is the whole chain
//! below the run — so one reported or stored as the other is a different number,
//! not an imprecise one. Only their being separate types prevents it: no
//! operation converts between them.
//!
//! That separation is what lets a consumer hold one without the other.
//! `zaino-chain-head` keeps a bounded window and never reads the finalised
//! state, so it cannot know the work below its window. It does not need to:
//! choosing between competing branches is a comparison among runs that begin at
//! the same block, which [`RelativeChainWork`]'s ordering answers on its own.
//! The absolute figure a client asks for is a separate job, answered elsewhere
//! from what the validator itself reports.
//!
//! # The algebra
//!
//! Write `W` for [`SingleBlockWork`], `C` for [`AbsoluteChainWork`], and `R`
//! for [`RelativeChainWork`]. Each relation is a method on the type it returns:
//!
//! ```text
//! W ∈ (0, 2^128)
//! C ∈ (0, 2^128)
//! R ∈ [0, 2^128)
//!
//! genesis    : W → C      a chain of one block
//! accumulate : C × W → C  extend the chain by one block
//! rollback   : C × W → C  unwind one block, on reorg
//! accumulate : R × W → R  extend a run by one block
//! ```
//!
//! Those four, with the orderings on `C` and on `R`, are the whole algebra.
//! `C × C` is not defined: no chain is the concatenation of two chains, so the
//! sum of two total chain works is not a quantity in this domain. `C` and `R`
//! do not meet either — combining a run with the absolute work below it would
//! be a relation between them, and none is defined because nothing needs one.
//!
//! Every fold is checked. Neither bound is reachable on a real chain; they stay
//! checked so a corrupt input fails loud instead of wrapping into a small value
//! that would then sort as a light chain. See ADR-0013 for the doctrine.
//!
//! Deriving [`SingleBlockWork`] from a difficulty target is not done here. The
//! nBits → target → work conversion is consensus logic, and belongs to a crate
//! that holds a consensus implementation. Those crates compute the integer and
//! pass it to [`SingleBlockWork::try_new`].

mod absolute_chain_work;
mod error;
mod relative_chain_work;
mod single_block_work;

pub use absolute_chain_work::{AbsoluteChainWork, ChainWorkOverWidth, WorkUnderflow};
pub use error::WorkOverflow;
pub use relative_chain_work::RelativeChainWork;
pub use single_block_work::{SingleBlockWork, ZeroWork};
