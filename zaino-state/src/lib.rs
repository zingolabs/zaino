//! Zaino's core mempool and chain-fetching Library.
//!
//! Built to use a configurable backend, current and planned backend options:
//! - FetchService (WIP - Functionality complete, requires local compact block cache for efficiency).
//!    - Built using the Zcash Json RPC Services for backwards compatibility with Zcashd and other JsonRPC based validators.
//! - StateService (WIP)
//!    - Built using Zebra's ReadStateService for efficient chain access.
//! - TonicService (PLANNED)
//! - DarksideService (PLANNED)

#![warn(missing_docs)]
#![forbid(unsafe_code)]

// Zaino's Indexer library frontend.
pub mod indexer;

// Core Indexer functionality
// NOTE: This should possibly be made pub(crate) and moved to the indexer mod.
pub mod fetch;
pub mod local_cache;
pub mod mempool;
pub mod remote_state;
pub mod state;

// Exposed backend Indexer functionality
pub mod config;
pub mod error;
pub mod status;
pub mod stream;

// Internal backend Indexer functionality.
pub(crate) mod broadcast;
pub(crate) mod utils;
