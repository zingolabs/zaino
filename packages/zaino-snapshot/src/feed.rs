//! `GetMempoolStream`'s source: one epoch per served tip, written once per transaction
//!
//! ```text
//!   served tip T ─▶ Epoch { opening: servable at T,  log: [tx₁, tx₂, …]  sealed? }
//!                            │ rendered once          │ each rendered once
//!   subscriber ─────────────▶ opening ───────────────▶ log[cursor..] ───▶ sealed = end
//!   served tip moves ─▶ new epoch opened, stored in the next snapshot, then the old one sealed
//! ```
//!
//! - Key = the served tip (G5): a stream's end ⇒ the next load serves the new tip
//! - Arrivals = the chain view's `arrivals` between two publishes (no per-tx pass)
//! - Never un-sends within an epoch (a tx gone mid-epoch stays in its log)

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock};

use bytes::Bytes;
use tokio::sync::watch;
use zaino_chainview::MempoolEntry;
use zaino_primitives::types::{BlockRef, TransactionId};

/// One transaction in an epoch's log, with the serving layer's wire form once rendered
#[derive(Debug)]
pub struct Logged {
    pub entry: MempoolEntry,
    rendered: OnceLock<Bytes>,
}

impl Logged {
    /// `render`: once per entry, first asker's (one wire form per serving layer)
    pub fn rendered(&self, render: impl FnOnce(&MempoolEntry) -> Bytes) -> Bytes {
        self.rendered.get_or_init(|| render(&self.entry)).clone()
    }
}

#[derive(Debug)]
struct Epoch {
    opening: Vec<MempoolEntry>,
    opening_rendered: OnceLock<Bytes>,
    log: RwLock<Log>,
}

/// `sent` = opening + appended txids (a re-crossing never logged twice)
#[derive(Debug, Default)]
struct Log {
    entries: Vec<Arc<Logged>>,
    sealed: bool,
    sent: HashSet<TransactionId>,
}

/// One snapshot's epoch: `key` = the served tip it opened at
#[derive(Debug, Clone)]
pub(crate) struct Feed {
    key: Option<BlockRef>,
    epoch: Arc<Epoch>,
    wake: watch::Receiver<()>,
}

impl Feed {
    pub(crate) fn open(
        key: Option<BlockRef>,
        opening: Vec<MempoolEntry>,
        wake: watch::Receiver<()>,
    ) -> Self {
        let sent = opening.iter().map(|entry| entry.txid).collect();
        let log = RwLock::new(Log { sent, ..Log::default() });
        let epoch = Epoch { opening, opening_rendered: OnceLock::new(), log };
        Self { key, epoch: Arc::new(epoch), wake }
    }

    pub(crate) fn key(&self) -> Option<BlockRef> {
        self.key
    }

    /// Appended unless sealed or already carried
    pub(crate) fn append(&self, entry: MempoolEntry) {
        let mut log = self.epoch.log.write().expect("feed log lock poisoned");
        if log.sealed || !log.sent.insert(entry.txid) {
            return;
        }
        log.entries.push(Arc::new(Logged { entry, rendered: OnceLock::new() }));
    }

    pub(crate) fn seal(&self) {
        self.epoch.log.write().expect("feed log lock poisoned").sealed = true;
    }

    pub(crate) fn sealed(&self) -> bool {
        self.epoch.log.read().expect("feed log lock poisoned").sealed
    }

    pub(crate) fn same_epoch(&self, other: &Feed) -> bool {
        Arc::ptr_eq(&self.epoch, &other.epoch)
    }

    pub(crate) fn tail(&self) -> MempoolTail {
        MempoolTail { epoch: Arc::clone(&self.epoch), cursor: 0, wake: self.wake.clone() }
    }
}

/// One `GetMempoolStream`: [`opening`](Self::opening), then [`next`](Self::next) until the served
/// tip moves
///
/// - cost per arrival: one read lock + one `Arc` clone; nothing grows with the epoch
#[derive(Debug)]
pub struct MempoolTail {
    epoch: Arc<Epoch>,
    cursor: usize,
    wake: watch::Receiver<()>,
}

impl MempoolTail {
    /// Servable mempool when the epoch opened, txid order
    pub fn opening(&self) -> &[MempoolEntry] {
        &self.epoch.opening
    }

    /// [`opening`](Self::opening) rendered once per epoch, shared by every tail on it
    pub fn opening_rendered(&self, render: impl FnOnce(&[MempoolEntry]) -> Bytes) -> Bytes {
        self.epoch.opening_rendered.get_or_init(|| render(&self.epoch.opening)).clone()
    }

    /// Next transaction turned servable after the opening, once; `None` = the served tip moved
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
            // `Err` = publisher dropped: no epoch moves again
            if self.wake.changed().await.is_err() {
                return None;
            }
        }
    }
}
