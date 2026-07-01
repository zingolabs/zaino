//! DAG-driven parallel sync engine for blockchain index building.
//!
//! Generic — contains no blockchain-specific knowledge. Blockchain-specific
//! index implementations and provisioners live in downstream crates.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod dag;
pub mod descriptor;
pub mod engine;
pub mod pipeline;
pub mod progress;
pub mod provisioner;
pub mod traits;
