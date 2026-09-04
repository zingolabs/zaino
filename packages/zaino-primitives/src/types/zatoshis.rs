//! Zcash monetary quantities in zatoshis.
//!
//! Three quantities share the zatoshi unit but not their invariants, so each is
//! its own type:
//!
//! - [`Zatoshis`] — an amount held, in `0 ..= supply`.
//! - [`ZatoshisFlowSum`] — an accumulation of movements, bounded only by machine
//!   representability and deliberately not by the supply.
//! - [`ZatoshisDelta`] — a change in a balance, in `-supply ..= supply`.
//!
//! Summing amounts as flow and differencing the totals into a delta are
//! relations between these types; they live in the [`arithmetic`] module
//! alongside the algebra that governs them. See ADR-0013.

mod amount;
mod arithmetic;
mod delta;
mod flow_sum;

pub use amount::{Zatoshis, ZatoshisOverflow};
pub use delta::{ZatoshisDelta, ZatoshisDeltaOverflow};
pub use flow_sum::ZatoshisFlowSum;

use amount::MAX_ZATOSHIS;
