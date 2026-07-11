//! DAG-driven parallel sync engine for blockchain index building.
//!
//! Generic — contains no blockchain-specific knowledge. Blockchain-specific
//! index implementations and provisioners live in downstream crates.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod block_buffer;
pub mod bridge;
pub mod dag;
pub mod descriptor;
pub mod engine;
pub mod index_set;
pub mod pipeline;
pub mod primitives;
pub mod progress;
pub mod scheduler;
pub(crate) mod encode;
pub mod provisioner;
pub mod traits;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
