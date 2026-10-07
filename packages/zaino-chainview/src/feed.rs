//! `GetMempoolStream`'s source: written once per transaction, read by cursors
//!
//! ```text
//!   tip block B ─▶ Epoch { opening: mempool at B,  log: [tx₁, tx₂, tx₃, …]  sealed? }
//!                            │ rendered once          │ each rendered once
//!   subscriber ─────────────▶ opening ───────────────▶ log[cursor..] ───▶ sealed = end
//!   (Arc<Epoch> + usize)        shared by refcount        shared by refcount
//!   next block ─▶ old epoch sealed (readers drain, then end) + new Epoch opened
//! ```
//!
//! - dedupe + render paid once (fold / first reader), never per subscriber
//! - never un-sends within a block (a tx gone mid-block stays in that block's log)

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock};

use bytes::Bytes;
use tokio::sync::watch;
use zaino_primitives::types::TransactionId;

use crate::snapshot::MempoolEntry;

/// One transaction in an epoch's log, with the serving layer's wire form once rendered
#[derive(Debug)]
pub struct Logged {
    pub entry: MempoolEntry,
    rendered: OnceLock<Bytes>,
}

impl Logged {
    /// `render` runs once per entry, whoever asks first (one wire form per serving layer)
    pub fn rendered(&self, render: impl FnOnce(&MempoolEntry) -> Bytes) -> Bytes {
        self.rendered.get_or_init(|| render(&self.entry)).clone()
    }
}

/// Servable mempool at one tip block, then each transaction turned servable after it
///
/// - `sealed` = the tip block moved (or the tip went unserved): readers drain, then end
#[derive(Debug)]
pub(crate) struct Epoch {
    opening: Vec<MempoolEntry>,
    opening_rendered: OnceLock<Bytes>,
    log: RwLock<Log>,
}

#[derive(Debug, Default)]
struct Log {
    entries: Vec<Arc<Logged>>,
    sealed: bool,
    /// Opening + appended txids (a re-crossing is never logged twice)
    sent: HashSet<TransactionId>,
}

impl Epoch {
    pub(crate) fn open(opening: Vec<MempoolEntry>) -> Self {
        let sent = opening.iter().map(|entry| entry.txid).collect();
        Self {
            opening,
            opening_rendered: OnceLock::new(),
            log: RwLock::new(Log { sent, ..Log::default() }),
        }
    }

    /// Appended unless this epoch already carried it
    pub(crate) fn append(&self, entry: MempoolEntry) {
        let mut log = self.log.write().expect("feed log lock poisoned");
        if log.sealed || !log.sent.insert(entry.txid) {
            return;
        }
        log.entries.push(Arc::new(Logged { entry, rendered: OnceLock::new() }));
    }

    pub(crate) fn seal(&self) {
        self.log.write().expect("feed log lock poisoned").sealed = true;
    }
}

/// One `GetMempoolStream`: [`opening`](Self::opening), then [`next`](Self::next) until the tip
/// block moves
///
/// - cost per arrival: one read lock + one `Arc` clone; nothing grows with the epoch
#[derive(Debug)]
pub struct MempoolTail {
    epoch: Arc<Epoch>,
    cursor: usize,
    wake: watch::Receiver<()>,
}

impl MempoolTail {
    /// `wake` subscribed before `epoch` was read (an append in between = one spurious wake)
    pub(crate) fn new(epoch: Arc<Epoch>, wake: watch::Receiver<()>) -> Self {
        Self { epoch, cursor: 0, wake }
    }

    /// The servable mempool at the tip block the stream opened on, in txid order
    pub fn opening(&self) -> &[MempoolEntry] {
        &self.epoch.opening
    }

    /// [`opening`](Self::opening) rendered once per epoch, shared by every tail on it
    pub fn opening_rendered(&self, render: impl FnOnce(&[MempoolEntry]) -> Bytes) -> Bytes {
        self.epoch.opening_rendered.get_or_init(|| render(&self.epoch.opening)).clone()
    }

    /// Next transaction turned servable after the opening, once; `None` = the tip block moved
    ///
    /// - cancel-safe: the cursor moves only when an entry is returned
    pub async fn next(&mut self) -> Option<Arc<Logged>> {
        loop {
            self.wake.borrow_and_update();
            {
                let log = self.epoch.log.read().expect("feed log lock poisoned");
                if let Some(logged) = log.entries.get(self.cursor) {
                    self.cursor += 1;
                    return Some(Arc::clone(logged));
                }
                if log.sealed {
                    return None;
                }
            }
            // sender lives in the core, which the epoch's writer holds: `Err` = view dropped
            if self.wake.changed().await.is_err() {
                return None;
            }
        }
    }

    /// Identity of the epoch (tails opened on one block share it)
    #[cfg(test)]
    pub(crate) fn same_epoch(&self, other: &MempoolTail) -> bool {
        Arc::ptr_eq(&self.epoch, &other.epoch)
    }
}
