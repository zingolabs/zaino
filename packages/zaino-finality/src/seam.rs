//! The seam and its two halves.

use std::sync::Arc;

use tokio::sync::watch;
use zaino_primitives::types::{BlockHash, Height};

use crate::fault::SeamFault;

/// The default retention overlap: how far below the durable watermark the
/// volatile tier keeps retaining, so the two tiers' ranges overlap and the
/// union of their coverage cannot gap.
pub const DEFAULT_RETENTION_MARGIN: u32 = 10;

/// The reorg horizon: heights at or below this are past the reorg window and
/// the durable tier may commit them.
///
/// Issued only by [`ReorgHorizon::advance`] — it has no public constructor, so
/// a holder cannot fabricate an authorisation it was not given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    height: Height,
    hash: BlockHash,
}

impl Released {
    /// The highest height past the reorg window.
    pub fn height(&self) -> Height {
        self.height
    }

    /// The canonical hash at that height, backed by the volatile tier's
    /// parent-linked chain down from its tip.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }
}

/// The durable watermark: heights at or below this are committed to disk.
///
/// Issued only by [`DurableWatermark::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    height: Height,
}

impl Committed {
    /// The highest height committed to disk.
    pub fn height(&self) -> Height {
        self.height
    }
}

/// The depths the seam owns, read once at construction.
#[derive(Debug, Clone, Copy)]
struct Depths {
    reorg: u32,
    retention_margin: u32,
}

/// The two legs of the ratchet, shared by both halves.
struct SeamState {
    depths: Depths,
    released: watch::Sender<Option<Released>>,
    committed: watch::Sender<Option<Committed>>,
}

/// The seam between the durable and volatile tiers.
///
/// Constructed once, before either tier, and immediately split: the channels
/// exist before the components, so neither tier has to be built before the
/// other. It is the only owner of the reorg depth and the retention margin.
pub struct Seam {
    state: Arc<SeamState>,
}

impl Seam {
    /// A seam over `reorg_depth` (the consensus reorg bound) and
    /// `retention_margin` (how far below the watermark the volatile tier keeps
    /// retaining, so the two tiers' ranges overlap and cannot gap).
    pub fn new(reorg_depth: u32, retention_margin: u32) -> Self {
        let (released, _) = watch::channel(None);
        let (committed, _) = watch::channel(None);
        Self {
            state: Arc::new(SeamState {
                depths: Depths {
                    reorg: reorg_depth,
                    retention_margin,
                },
                released,
                committed,
            }),
        }
    }

    /// The two halves: one per tier, each owning the quantity it publishes.
    pub fn split(self) -> (ReorgHorizon, DurableWatermark) {
        let committed_rx = self.state.committed.subscribe();
        let released_rx = self.state.released.subscribe();
        (
            ReorgHorizon {
                state: Arc::clone(&self.state),
                committed: committed_rx,
            },
            DurableWatermark {
                state: self.state,
                released: released_rx,
            },
        )
    }
}

/// The volatile tier's half. Owns the reorg horizon, reads the watermark.
///
/// Not `Clone`: a second holder would be a second publisher of the horizon,
/// which is the drift the seam exists to prevent.
pub struct ReorgHorizon {
    state: Arc<SeamState>,
    committed: watch::Receiver<Option<Committed>>,
}

impl ReorgHorizon {
    /// The consensus reorg depth, so the caller knows which height's hash to
    /// supply to [`advance`](Self::advance).
    pub fn reorg_depth(&self) -> u32 {
        self.state.depths.reorg
    }

    /// Publishes the horizon derived from `tip`.
    ///
    /// The caller supplies its verified tip and the canonical hash of the block
    /// `reorg_depth` below it, never the horizon itself: the seam applies the
    /// derivation, so a horizon inside the reorg window cannot be named.
    pub fn advance(
        &mut self,
        tip: Height,
        hash_at_horizon: BlockHash,
    ) -> Result<Released, SeamFault> {
        let to = tip.saturating_sub(self.state.depths.reorg);
        if let Some(held) = *self.state.released.borrow() {
            if u32::from(to) < u32::from(held.height) {
                return Err(SeamFault::RegressedHorizon {
                    to,
                    held: held.height,
                });
            }
        }
        let released = Released {
            height: to,
            hash: hash_at_horizon,
        };
        self.state.released.send_replace(Some(released));
        Ok(released)
    }

    /// The durable tier's watermark, or `None` while it holds nothing.
    pub fn durable(&self) -> Option<Committed> {
        *self.committed.borrow()
    }

    /// The lowest height this tier must keep retaining: the watermark less the
    /// retention margin, saturating at genesis. `None` while the durable tier
    /// holds nothing, when it must retain everything it has.
    ///
    /// Computed here so the margin has exactly one reader.
    pub fn retention_floor(&self) -> Option<Height> {
        self.durable().map(|committed| {
            committed
                .height
                .saturating_sub(self.state.depths.retention_margin)
        })
    }
}

/// The durable tier's half. Owns the watermark, reads the horizon.
///
/// Not `Clone`, for the same reason as [`ReorgHorizon`].
pub struct DurableWatermark {
    state: Arc<SeamState>,
    released: watch::Receiver<Option<Released>>,
}

impl DurableWatermark {
    /// A cloneable, read-only view of the reorg horizon this half publishes
    /// against.
    ///
    /// Reading the horizon is not a capability the seam needs to ration — only
    /// *publishing* the watermark must stay single-owner. A holder of this reader
    /// can observe the horizon (for a progress target, say) without being able to
    /// advance the watermark, which still requires the [`DurableWatermark`].
    pub fn reader(&self) -> HorizonReader {
        HorizonReader {
            state: Arc::clone(&self.state),
            released: self.state.released.subscribe(),
        }
    }

    /// The current horizon, or `None` while the volatile tier has published none.
    pub fn released(&self) -> Option<Released> {
        *self.released.borrow()
    }

    /// Resolves on the next horizon the volatile tier publishes.
    ///
    /// A `watch`, so a caller that falls behind skips to the present rather than
    /// replaying every intermediate horizon.
    pub async fn await_released(&mut self) -> Released {
        loop {
            if self.released.changed().await.is_err() {
                // The seam outlives both halves (both hold an Arc), so the
                // sender cannot drop while this receiver lives.
                unreachable!("the seam state outlives this half");
            }
            if let Some(released) = *self.released.borrow_and_update() {
                return released;
            }
        }
    }

    /// Publishes the watermark reached under `authorised_by`.
    ///
    /// Requires the horizon that authorised the work, so advancing on this
    /// tier's own authority is unrepresentable rather than merely rejected.
    pub fn advance(
        &mut self,
        authorised_by: &Released,
        to: Height,
    ) -> Result<Committed, SeamFault> {
        if u32::from(to) > u32::from(authorised_by.height) {
            return Err(SeamFault::WatermarkPastHorizon {
                to,
                horizon: authorised_by.height,
            });
        }
        if let Some(held) = *self.state.committed.borrow() {
            if u32::from(to) < u32::from(held.height) {
                return Err(SeamFault::RegressedWatermark {
                    to,
                    held: held.height,
                });
            }
        }
        let committed = Committed { height: to };
        self.state.committed.send_replace(Some(committed));
        Ok(committed)
    }
}

/// A read-only view of the reorg horizon.
///
/// `Clone`: many parties may observe the horizon, but only the holder of
/// [`DurableWatermark`] may publish against it. Reading is not a capability the
/// seam needs to ration, so this reader carries no authority — it exposes the
/// horizon and nothing else. Obtained from [`DurableWatermark::reader`].
#[derive(Clone)]
pub struct HorizonReader {
    /// Held only to keep the shared state — and so the publishing sender — alive
    /// for the reader's lifetime, so [`await_released`](Self::await_released) can
    /// treat a closed channel as unreachable rather than a value it cannot
    /// produce. Never read directly.
    #[allow(dead_code)]
    state: Arc<SeamState>,
    released: watch::Receiver<Option<Released>>,
}

impl HorizonReader {
    /// The current horizon, or `None` while the volatile tier has published none.
    pub fn released(&self) -> Option<Released> {
        *self.released.borrow()
    }

    /// Resolves on the next horizon the volatile tier publishes.
    ///
    /// A `watch`, so a reader that falls behind skips to the present rather than
    /// replaying every intermediate horizon.
    pub async fn await_released(&mut self) -> Released {
        loop {
            if self.released.changed().await.is_err() {
                // This reader holds the seam state, so the publishing sender
                // cannot drop while the reader lives.
                unreachable!("the seam state outlives this reader");
            }
            if let Some(released) = *self.released.borrow_and_update() {
                return released;
            }
        }
    }
}
