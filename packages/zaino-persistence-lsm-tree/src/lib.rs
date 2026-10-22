#![doc = include_str!("../usage.md")]
// only `unsafe` = `fs::sys` (mmap, sync_file_range, mincore, ioprio)
#![deny(unsafe_code)]
// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap
    )
)]

mod backend;
mod buffer;
mod dir;
mod emit;
mod engine;
mod error;
pub mod fs;
mod lsm;
mod manifest;
mod pages;
mod sequence;
#[cfg(test)]
mod testing;

pub use backend::{verify_dir, LsmTreeBackend, LsmTreeConfig, LsmTreeReader, LsmTreeWriter};
pub use dir::disk_bytes;
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use lsm::{LsmConfig, Tuning, METRIC_BUCKETS as LSM_METRIC_BUCKETS};
