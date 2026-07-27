//! Zebra ReadState adapter — direct database reads, no RPC.
//!
//! Opens Zebra's finalized state database read-only and implements
//! zaino-source query traits. Orders of magnitude faster than RPC
//! for bulk sync — no HTTP, no hex encoding, no JSON parsing.
//!
//! ```ignore
//! let adapter = ZebraReadStateAdapter::open(
//!     "/var/cache/zebrad-cache",
//!     Network::Mainnet,
//! ).await.expect("open zebra state");
//! let block = adapter.get_block(height).await?;
//! ```

mod adapter;

pub use adapter::ZebraReadStateAdapter;
