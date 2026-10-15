//! Peers' block `inv`s into the view (which chain each live peer carries: peer finality)
//!
//! ```text
//!   PeerSource::heard ─▶ batch ─(each PEER_FOLD)─▶ fold: live peer → its latest block
//! ```
//!
//! - transaction `inv`s never reach the view: mempool membership = each peer's listing
//!   (`peer_mempool.rs`)

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zaino_primitives::types::Announced;

use crate::fold::ChainViewCore;
use crate::ports::PeerSource;

/// Announcements folded at most this often (one publish per batch, not per `inv`)
pub(crate) const PEER_FOLD: Duration = Duration::from_millis(250);

/// Feeds the view from the peers' block announcements until cancelled
pub struct PeerWatch {
    core: Arc<ChainViewCore>,
    peers: Arc<dyn PeerSource>,
}

impl PeerWatch {
    pub(crate) fn new(core: Arc<ChainViewCore>, peers: Arc<dyn PeerSource>) -> Self {
        Self { core, peers }
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut heard = self.peers.heard();
        let mut fold = tokio::time::interval(PEER_FOLD);
        fold.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut batch: Vec<Announced> = Vec::new();
        let mut ended = false;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                next = heard.next(), if !ended => match next {
                    Some(announced) => batch.push(announced),
                    None => {
                        warn!("Peer announcements ended, peer finality frozen");
                        ended = true;
                    }
                },
                _ = fold.tick() => {
                    self.core.apply_heard(std::mem::take(&mut batch), self.peers.live());
                }
            }
        }
    }
}
