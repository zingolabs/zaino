//! Zcash monetary quantities in zatoshis
//!
//! - [`Zatoshis`]: an amount, `0 ..= supply`
//! - [`SignedZatoshis`]: a movement or difference, `-supply ..= supply`

mod amount;
mod signed;

pub use amount::{Zatoshis, ZatoshisOverflow};
pub use signed::{SignedZatoshis, SignedZatoshisOverflow};

use amount::MAX_ZATOSHIS;
