//! One `Index on disk` line per enabled index every [`INTERVAL`] while it bulk syncs: durable
//! tip + bytes on disk; silent while it serves

use std::{path::PathBuf, time::Duration};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use zaino_persistence::{dir::disk_bytes, lsm::Size};
use zaino_primitives::types::Height;

use crate::error::IndexerError;

const INTERVAL: Duration = Duration::from_secs(120);

/// Until `cancel`; `finalized` = the index's durable tip, `synced` = its serving gate, `dir` = its
/// directory
pub(crate) async fn run(
    finalized: watch::Receiver<Option<Height>>,
    mut synced: watch::Receiver<bool>,
    dir: PathBuf,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    loop {
        let follower_gone = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            syncing = synced.wait_for(|serving| !serving) => syncing.is_err(),
        };
        if follower_gone {
            cancel.cancelled().await;
            return Ok(());
        }
        let mut ticks = tokio::time::interval(INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                _ = synced.wait_for(|serving| *serving) => break,
                _ = ticks.tick() => {}
            }
            let durable = (*finalized.borrow()).map(u32::from);
            let walk = dir.clone();
            match tokio::task::spawn_blocking(move || disk_bytes(&walk)).await? {
                Ok(bytes) => info!(durable, size = %Size(bytes), "Index on disk"),
                Err(error) => warn!(durable, %error, "Index size unreadable"),
            }
        }
    }
}
