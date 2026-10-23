//! What every Zaino index stores through: persistence port (`port.rs`) + its engine,
//! [`DiskEngine`] (guide: `usage.md`)

// only `unsafe`: `fs::real` (mmap, sync_file_range, mincore), `fs::probe` (FIEMAP ioctl),
// `thread` (ioprio_set, setpriority)
#![deny(unsafe_code)]

mod changes;
mod disk;
mod error;
pub mod fs;
#[cfg(test)]
mod golden;
mod index_dir;
mod lsm;
mod manifest;
mod metrics;
mod overlay;
mod pages;
mod port;
mod schema;
mod sequence;
mod thread;
mod write_buffer;

pub use self::metrics::METRICS;
pub use changes::{BlockChanges, MapChanges, SequenceAppends};
pub use disk::{manifest_fits, DiskEngine, DiskStore, DiskView, TableFootprint};
pub use error::{CommitFailed, StoreError};
pub use lsm::{Fanout, LsmConfig, SegmentError};
pub use manifest::ManifestError;
pub use overlay::{AppliedLayer, Entry, Overlay, OverlayView};
pub use pages::PageError;
pub use port::{
    Checked, MapRead, MapView, PersistenceEngine, PinnedView, SequenceRead, SequenceView, Store,
    Verification, View,
};
pub use schema::{IndexKind, MapId, MapTable, Schema, SequenceId, SequenceTable, Tables, Width};
pub use write_buffer::{AppliedView, WriteBuffer};
