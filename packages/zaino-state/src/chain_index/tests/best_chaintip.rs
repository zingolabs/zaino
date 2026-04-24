//! Regression tests for `best_chaintip`.
//!
//! Tracks <https://github.com/zingolabs/zaino/issues/1047>: when the snapshot
//! is `StillSyncingFinalizedState` and the source cannot currently produce
//! the block at `validator_finalized_height`, `best_chaintip` surfaces the
//! miss as `database_hole` — the corruption kind — instead of a retryable
//! "not ready" signal.

use super::load_test_vectors_and_sync_chain_index;
use crate::chain_index::ChainIndex;
use crate::{ChainIndexSnapshot, Height};

/// Reaches the `StillSyncingFinalizedState` arm with a
/// `validator_finalized_height` the source cannot serve, mirroring the
/// cold-boot race where zebrad transiently can't produce block `tip - 100`.
/// The error (if any) must not be a `database_hole`: that category means
/// zaino's own on-disk index claims a height whose bytes are unreachable,
/// which is not what happened here.
#[tokio::test(flavor = "multi_thread")]
async fn passthrough_miss_is_not_a_database_hole() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;

    let above_tip = Height(mockchain.max_chain_height() + 1_000);
    let fake_snapshot = ChainIndexSnapshot::StillSyncingFinalizedState {
        validator_finalized_height: above_tip,
    };

    match index_reader.best_chaintip(&fake_snapshot).await {
        Ok(_tip) => {}
        Err(e) => {
            let rendered = e.to_string();
            assert!(
                !rendered.contains("hole in validator database"),
                "best_chaintip misclassified a passthrough miss as a database hole: {rendered}",
            );
        }
    }
}
