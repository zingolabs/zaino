#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod chain;
mod header;
mod params;
mod rules;
mod snapshot;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use chain::{BestTip, HeaderChain, Inserted, Record};
pub use header::{decode_header, DecodeError, Header};
pub use params::Params;
pub use rules::Rejected;
pub use snapshot::HeaderChainSnapshot;
