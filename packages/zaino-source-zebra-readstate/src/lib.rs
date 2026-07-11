//! Zebra ReadState adapter — direct database reads, no RPC.
//!
//! Opens Zebra's finalized state database read-only and implements
//! zaino-source query traits against it. Orders of magnitude faster
//! than the RPC adapter for bulk sync — no HTTP, no hex encoding,
//! no JSON parsing.
//!
//! ```ignore
//! let adapter = ZebraReadStateAdapter::open(
//!     "/var/cache/zebrad-cache/state/v27/mainnet",
//!     Network::Mainnet,
//! ).expect("open zebra state");
//! let block = adapter.get_block(height).await?;
//! ```

mod adapter;
mod convert;

pub use adapter::ZebraReadStateAdapter;
