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
// the engine that uses these lands in later commits of this series
#![allow(dead_code, unused_imports)]

mod dir;
pub mod fs;
mod lsm;
mod manifest;
mod pages;

pub use dir::disk_bytes;
pub use lsm::{describe_metrics, LsmConfig, Tuning, METRIC_BUCKETS};
