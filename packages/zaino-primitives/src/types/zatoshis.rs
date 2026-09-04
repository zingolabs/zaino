//! Zcash monetary quantities in zatoshis.
//!
//! A zatoshi is one unit, but *summing* zatoshis is not one operation. What the
//! same amounts mean depends on what is being summed: a set of balances that
//! coexist on chain at one moment cannot total more than the money supply, while
//! the movements through an address over history — every receipt and every
//! spend — count the same coins each time they move and have no such bound. The
//! invariant of a sum is a property of what is summed, not of the zatoshi or of
//! `+`. So each quantity is its own type, and each summation lands in the type
//! whose invariant it satisfies:
//!
//! - [`Zatoshis`] — an amount held, in `0 ..= supply`.
//! - [`ZatoshisFlowSum`] — an accumulation of movements, bounded only by machine
//!   representability and deliberately not by the supply.
//! - [`SignedZatoshis`] — a signed value (a movement or a difference), in
//!   `-supply ..= supply`.
//!
//! Summing amounts as flow and subtracting one flow total from another are
//! relations between these types; they live in the [`arithmetic`] module
//! alongside the algebra that governs them. See ADR-0013.

mod amount;
mod arithmetic;
mod flow_sum;
mod signed;

pub use amount::{Zatoshis, ZatoshisOverflow};
pub use flow_sum::ZatoshisFlowSum;
pub use signed::{SignedZatoshis, SignedZatoshisOverflow};

use amount::MAX_ZATOSHIS;
