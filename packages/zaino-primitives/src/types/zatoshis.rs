//! Zcash monetary quantities in zatoshis
//!
//! - [`Zatoshis`]: an amount, `0` to `supply`, both inclusive
//! - [`SignedZatoshis`]: a movement or difference, `-supply` to `supply`, both inclusive

mod amount;
mod signed;

pub use amount::{Zatoshis, ZatoshisOverflow};
pub use signed::{SignedZatoshis, SignedZatoshisOverflow};

use amount::MAX_ZATOSHIS;
