//! `getinfo` + `getdeprecationinfo`: which release a validator runs, and when it halts

use super::Height;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRelease {
    /// e.g. `v6.4.2`
    pub build: String,
    /// p2p user agent, e.g. `/Zebra:6.4.2/`
    pub user_agent: String,
    pub protocol_version: u32,
    pub end_of_service: EndOfService,
}

/// When this release stops itself (zebrad halts once its tip passes `height`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOfService {
    /// `estimated_unix` = zebrad's own guess, a day early
    At { height: Height, estimated_unix: i64 },
    /// Not enforced on this network (mainnet only)
    NotEnforced,
    /// Release predates `getdeprecationinfo` (zebrad < 6.3)
    Unknown,
}
