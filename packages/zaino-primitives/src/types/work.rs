//! Proof-of-work quantities.
//!
//! Work is one unit, but two quantities are measured in it:
//!
//! - [`SingleBlockWork`] — the work one block is expected to take, derived
//!   from its difficulty target.
//! - [`AbsoluteChainWork`] — the total work of a chain up to and including a
//!   block. This is the value validators report as `chainwork`.
//!
//! They are separate types because merging them would let two wrong statements
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
//! A third work quantity exists in the workspace: work accumulated since an
//! anchor block rather than since genesis, which `zaino-chain-head` models as
//! its crate-local `ChainHeadWork`. It is deliberately not
//! [`AbsoluteChainWork`]. The two are measured from different origins, so a
//! value of one served or compared as the other is wrong by the difference
//! between the anchor and genesis. Promoting it to a primitive, and with it the
//! relations that convert between the origins, is a planned follow-up.
//!
//! The relations that fold one quantity into the other live in the `arithmetic`
//! module, which also states the whole algebra. See ADR-0013 for the doctrine.
//!
//! Deriving [`SingleBlockWork`] from a difficulty target is not done here. The
//! nBits → target → work conversion is consensus logic, and belongs to a crate
//! that holds a consensus implementation. Those crates compute the integer and
//! pass it to [`SingleBlockWork::try_new`].

mod absolute_chain_work;
mod arithmetic;
mod single_block_work;

pub use absolute_chain_work::{AbsoluteChainWork, ChainWorkOverWidth};
pub use arithmetic::{WorkOverflow, WorkUnderflow};
pub use single_block_work::{SingleBlockWork, ZeroWork};
