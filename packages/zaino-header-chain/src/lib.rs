#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod chain;
mod header;
mod params;
mod rules;
mod store;
mod target;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod verified;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use chain::{BestTip, HeaderChain, Inserted};
pub use header::{decode_header, DecodeError, Header};
pub use params::Params;
pub use rules::{check, link_run, Checked, Rejected};
pub use store::{HeaderStore, Record, FORMAT, TABLES};
pub use verified::{Fork, VerifiedChain};
