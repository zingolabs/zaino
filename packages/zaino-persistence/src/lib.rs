//! Storage core implementing an LSM-Tree Index: the file layer, the manifest commit point,
//! immutable sorted segments, and the offline verifier's report types (`docs/design/durability.md`)

// only `unsafe` = `fs::real::{map_read_only, start_writeback}` (mmap, sync_file_range)
#![deny(unsafe_code)]

pub mod dir;
pub mod fs;
pub mod lsm;
pub mod manifest;
pub mod pages;

/// Why an index directory could not be opened, read, proven or committed
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("index io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),

    #[error(transparent)]
    Page(#[from] pages::PageError),

    #[error(transparent)]
    Segment(#[from] lsm::SegmentError),
}
