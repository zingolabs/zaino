//! LMDB-backed best-chain store — append-only, keyed by height.
//!
//! Keys are 4-byte big-endian heights. Each value is a 32-byte hash followed
//! by opaque block payload bytes. A sentinel key `[0xFF; 4]`
//! stores the block count `cs = len(freezer.blocks)` for recovery.

use std::path::Path;

use lmdb::{Database, DatabaseFlags, Environment, Transaction, WriteFlags};

use crate::error::StoreError;
use crate::types::{Block, BlockHash, Height};

/// Sentinel key for storing the block count (= `len(freezer.blocks)` = `cs`).
const BLOCK_COUNT_KEY: [u8; 4] = [0xFF; 4];

pub struct LmdbStore {
    pub(crate) env: Environment,
    pub(crate) db: Database,
}

impl LmdbStore {
    /// Open or create the LMDB database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(path)
            .map_err(|e| StoreError::FreezeError(format!("mkdir {path:?}: {e}")))?;

        let env = Environment::new()
            .set_max_dbs(1)
            .set_map_size(512 * 1024 * 1024 * 1024) // 512 GB max
            .open(path)
            .map_err(|e| StoreError::FreezeError(format!("lmdb open: {e}")))?;

        let db = match env.open_db(Some("blocks")) {
            Ok(db) => db,
            Err(lmdb::Error::NotFound) => env
                .create_db(Some("blocks"), DatabaseFlags::empty())
                .map_err(|e| StoreError::FreezeError(format!("lmdb create_db: {e}")))?,
            Err(e) => return Err(StoreError::FreezeError(format!("lmdb open_db: {e}"))),
        };

        let store = Self { env, db };
        let count = store.block_count()?.unwrap_or(0);
        tracing::info!(path = %path.display(), block_count_on_open = count, "LMDB opened");
        Ok(store)
    }

    /// Write a batch of blocks in a single transaction. Each entry is
    /// (hash, block). The block's payload bytes are stored alongside the
    /// hash. Also updates the block-count sentinel to `last_height + 1`.
    pub fn put_batch(&self, batch: &[(BlockHash, Block)]) -> Result<(), StoreError> {
        if batch.is_empty() {
            return Ok(());
        }

        let mut txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| StoreError::FreezeError(format!("lmdb begin_rw_txn: {e}")))?;

        for (ref hash, ref block) in batch {
            let key = block.height.to_be_bytes();
            let mut value = Vec::with_capacity(64 + block.data.len());
            value.extend_from_slice(hash);
            value.extend_from_slice(&block.prev_hash);
            value.extend_from_slice(&block.data);
            txn.put(self.db, &key, &value, WriteFlags::empty())
                .map_err(|e| StoreError::FreezeError(format!("lmdb put: {e}")))?;
        }

        // Store block count (= last_height + 1) so recovery can set cs
        // directly from the sentinel without a +1 adjustment.
        let block_count = batch.last().unwrap().1.height + 1;
        txn.put(
            self.db,
            &BLOCK_COUNT_KEY,
            &block_count.to_be_bytes(),
            WriteFlags::empty(),
        )
        .map_err(|e| StoreError::FreezeError(format!("lmdb put block_count: {e}")))?;

        txn.commit()
            .map_err(|e| StoreError::FreezeError(format!("lmdb commit: {e}")))?;
        self.env
            .sync(true)
            .map_err(|e| StoreError::FreezeError(format!("lmdb sync: {e}")))?;

        tracing::debug!(count = batch.len(), "LMDB wrote batch");
        Ok(())
    }

    /// Read a block at a height. Returns the (hash, Block) with
    /// payload bytes ready for decoding by the caller.
    pub fn get(&self, height: Height) -> Result<Option<(BlockHash, Block)>, StoreError> {
        let key = height.to_be_bytes();
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| StoreError::FreezeError(format!("lmdb begin_ro_txn: {e}")))?;
        let value = match txn.get(self.db, &key) {
            Ok(v) => v,
            Err(lmdb::Error::NotFound) => return Ok(None),
            Err(e) => return Err(StoreError::FreezeError(format!("lmdb get: {e}"))),
        };
        if value.len() < 64 {
            return Ok(None);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&value[..32]);
        let mut prev_hash = [0u8; 32];
        prev_hash.copy_from_slice(&value[32..64]);
        let data = value[64..].to_vec();
        Ok(Some((hash, Block::new(height, hash, prev_hash, data))))
    }

    /// Delete all entries at heights strictly above `max_height`, and update
    /// the block-count sentinel to `max_height + 1`. Returns the number of
    /// deleted entries.
    ///
    /// This is used to trim a corrupted chain back to a known-good height
    /// before restarting sync.
    pub fn truncate_to_height(&self, max_height: Height) -> Result<usize, StoreError> {
        let latest = match self.block_count()? {
            Some(c) if c > 0 => c - 1,
            _ => return Ok(0),
        };

        if max_height >= latest {
            return Ok(0);
        }

        let mut txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| StoreError::FreezeError(format!("lmdb begin_rw_txn: {e}")))?;

        let mut deleted = 0usize;
        for h in (max_height + 1)..=latest {
            let key = h.to_be_bytes();
            match txn.del(self.db, &key, None) {
                Ok(()) => deleted += 1,
                Err(lmdb::Error::NotFound) => {} // already gone, not an error
                Err(e) => {
                    return Err(StoreError::FreezeError(format!(
                        "lmdb del height {h}: {e}"
                    )))
                }
            }
        }

        // Store block count (= max_height + 1).
        let block_count = max_height + 1;
        txn.put(
            self.db,
            &BLOCK_COUNT_KEY,
            &block_count.to_be_bytes(),
            WriteFlags::empty(),
        )
        .map_err(|e| StoreError::FreezeError(format!("lmdb put block_count: {e}")))?;

        txn.commit()
            .map_err(|e| StoreError::FreezeError(format!("lmdb commit: {e}")))?;
        self.env
            .sync(true)
            .map_err(|e| StoreError::FreezeError(format!("lmdb sync: {e}")))?;

        tracing::info!(
            old_count = latest + 1,
            new_count = block_count,
            deleted,
            "LMDB truncated"
        );
        Ok(deleted)
    }

    /// Return the number of blocks in the freezer (`cs`).
    ///
    /// Reads the sentinel key. Returns `None` if the database is empty (no
    /// blocks written yet).
    pub fn block_count(&self) -> Result<Option<Height>, StoreError> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| StoreError::FreezeError(format!("lmdb begin_ro_txn: {e}")))?;
        match txn.get(self.db, &BLOCK_COUNT_KEY) {
            Ok(bytes) if bytes.len() >= 4 => {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(&bytes[..4]);
                Ok(Some(Height::from_be_bytes(arr)))
            }
            Ok(_) => Ok(None),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(StoreError::FreezeError(format!("lmdb block_count: {e}"))),
        }
    }
}

impl std::fmt::Debug for LmdbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.block_count().ok().flatten().unwrap_or(0);
        f.debug_struct("LmdbStore").field("block_count", &count).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GENESIS_HASH;

    #[test]
    fn truncate_to_height_removes_blocks_above_max() -> Result<(), StoreError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = LmdbStore::open(tmp.path())?;

        // Write blocks at heights 0..=5 → block count = 6.
        let batch: Vec<_> = (0..=5u32)
            .map(|h| {
                let hash = [h as u8; 32];
                let prev = if h == 0 { GENESIS_HASH } else { [(h - 1) as u8; 32] };
                (hash, Block::new(h, hash, prev, vec![h as u8]))
            })
            .collect();
        store.put_batch(&batch)?;
        assert_eq!(store.block_count()?.unwrap(), 6);

        // Truncate to height 2: blocks 3,4,5 deleted.  New count = 3.
        let deleted = store.truncate_to_height(2)?;
        assert_eq!(deleted, 3);
        assert_eq!(store.block_count()?.unwrap(), 3);

        // Blocks at or below max still present.
        assert!(store.get(0)?.is_some());
        assert!(store.get(1)?.is_some());
        assert!(store.get(2)?.is_some());
        // Blocks above max gone.
        assert!(store.get(3)?.is_none());
        assert!(store.get(4)?.is_none());
        assert!(store.get(5)?.is_none());

        // Truncate above latest is no-op.
        let deleted = store.truncate_to_height(10)?;
        assert_eq!(deleted, 0);
        Ok(())
    }
}
