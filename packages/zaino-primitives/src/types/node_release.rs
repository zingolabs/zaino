//! `getinfo` + `getdeprecationinfo`: which release a validator runs, and when it halts

use super::Height;

/// `build` e.g. `v6.4.2`; `user_agent` = p2p, e.g. `/Zebra:6.4.2/`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRelease {
    pub build: String,
    pub user_agent: String,
    pub protocol_version: u32,
    pub end_of_service: EndOfService,
}

/// When this release stops itself (zebrad halts once its tip passes `height`)
///
/// - `estimated_unix` = zebrad's own guess, a day early
/// - `NotEnforced` = this network (mainnet only); `Unknown` = zebrad < 6.3 (no
///   `getdeprecationinfo`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOfService {
    At { height: Height, estimated_unix: i64 },
    NotEnforced,
    Unknown,
}
