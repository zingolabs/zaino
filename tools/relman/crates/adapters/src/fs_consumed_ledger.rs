use std::path::PathBuf;

use relman_core::ports::{ConsumedLedgerStore, ConsumedLedgerStoreError};
use relman_core::types::ConsumedLedger;

/// A [`ConsumedLedgerStore`] over a single real file (the resolved
/// `consumed_ledger` path).
///
/// `read` parses the file into a [`ConsumedLedger`], treating an absent file as
/// an empty ledger; `write` renders the ledger to TOML and creates parent
/// directories as needed. Unlike the changeset store, this one parses — the port
/// returns the domain type, so the parse lives at the boundary.
pub struct FsConsumedLedgerStore {
    path: PathBuf,
}

impl FsConsumedLedgerStore {
    /// Root the store at `path`. The file need not exist yet — `read` reports an
    /// empty ledger until `write` creates it.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn io_err(&self, source: std::io::Error) -> ConsumedLedgerStoreError {
        ConsumedLedgerStoreError::Io {
            path: self.path.display().to_string(),
            source,
        }
    }
}

impl ConsumedLedgerStore for FsConsumedLedgerStore {
    fn read(&self) -> Result<ConsumedLedger, ConsumedLedgerStoreError> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            // A missing ledger file is an empty ledger, not an error.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConsumedLedger::default());
            }
            Err(source) => return Err(self.io_err(source)),
        };
        ConsumedLedger::parse_toml(&contents).map_err(|source| ConsumedLedgerStoreError::Parse {
            path: self.path.display().to_string(),
            source,
        })
    }

    fn write(&self, ledger: &ConsumedLedger) -> Result<(), ConsumedLedgerStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| self.io_err(source))?;
        }
        std::fs::write(&self.path, ledger.to_toml()).map_err(|source| self.io_err(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_core::types::{CycleId, Uid};

    fn uid(raw: &str) -> Uid {
        Uid::parse(raw).expect("valid test uid")
    }

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid test cycle id")
    }

    const SAMPLE_UID: &str = "018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f";

    #[test]
    fn read_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsConsumedLedgerStore::new(dir.path().join("nested/consumed-ledger.toml"));
        assert!(store.read().expect("read").is_empty());
    }

    #[test]
    fn write_then_read_round_trips_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsConsumedLedgerStore::new(dir.path().join("nested/consumed-ledger.toml"));

        let mut ledger = ConsumedLedger::default();
        ledger.insert(uid(SAMPLE_UID), cycle("cycle-1"), Some("pr-9".to_owned()));
        store.write(&ledger).expect("write into missing dir");

        let read_back = store.read().expect("read back");
        assert_eq!(read_back, ledger);
        assert!(read_back.contains(&uid(SAMPLE_UID)));
    }
}
