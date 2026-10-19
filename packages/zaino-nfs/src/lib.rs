#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod fold;
mod graph;
mod indexes;
mod metrics;
mod nfs;
mod nfs_core;
mod per_index;
mod publish;

pub use crate::fold::FoldError;
pub use crate::indexes::schema;
pub use crate::metrics::describe_metrics;
pub use crate::nfs::{Nfs, NfsError};
pub use crate::publish::{ChainParams, IndexesAtBlock, NfsPublication, Published, Views};
