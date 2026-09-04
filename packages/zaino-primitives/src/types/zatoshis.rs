//! Zcash monetary quantities in zatoshis.

mod amount;
mod delta;

pub use amount::{Zatoshis, ZatoshisOverflow};
pub use delta::{ZatoshisDelta, ZatoshisDeltaOverflow};

use amount::MAX_ZATOSHIS;
