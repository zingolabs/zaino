//! Snapshot coherence (US-0.1) — the architecture's defining invariant.
//!
//! A pinned snapshot must keep answering as of the instant it was taken, even as
//! the live NFS window advances underneath it. The other mocks are static, so
//! this test uses an NFS whose *live* tip is mutable: it pins a snapshot, moves
//! the live tip, and asserts the pinned view is unchanged while a fresh snapshot
//! sees the advance. This guards against a regression where the read-context
//! re-queries live state instead of holding what it captured.

#[path = "support/mocks.rs"]
mod mocks;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_nfs::{FollowError, FrozenOut, NfsView, NonFinalisedState};
use zaino_runtime::{RuntimeBuilder, RuntimeConfig};
use zaino_service::BlockRead;
use zaino_core::TipEvent;

use mocks::{block_id, h, Calls, MockFs, MockNfsSnap, MockSource};

/// An NFS whose live tip can advance after construction. `snapshot()` reads the
/// current tip into an immutable [`MockNfsSnap`] — modelling the real `Chain`,
/// where a snapshot is a cheap pin of the window as it is *right now*.
struct LiveNfs {
    live_tip: Arc<AtomicU32>,
    finalised: u32,
    calls: Calls,
}

impl NonFinalisedState for LiveNfs {
    type Snapshot = MockNfsSnap;

    fn snapshot(&self) -> NfsView<MockNfsSnap> {
        let tip = self.live_tip.load(Ordering::SeqCst);
        NfsView::Ready(MockNfsSnap {
            tip: block_id(tip, 0xAA),
            range: (h(self.finalised + 1), h(tip)),
            calls: self.calls.clone(),
        })
    }
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent> {
        stream::empty().boxed()
    }
    fn frozen(&self) -> BoxStream<'_, FrozenOut> {
        stream::empty().boxed()
    }
    async fn follow<S: Send + Sync>(&self, _source: &S) -> Result<(), FollowError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_snapshot_is_pinned_across_live_advancement() {
    let calls = Calls::default();
    let live_tip = Arc::new(AtomicU32::new(150));
    let fs = MockFs {
        watermark: h(100),
        calls: calls.clone(),
    };
    let nfs = LiveNfs {
        live_tip: Arc::clone(&live_tip),
        finalised: 100,
        calls: calls.clone(),
    };
    let source = MockSource {
        calls: calls.clone(),
    };
    let runtime = RuntimeBuilder::new()
        .config(RuntimeConfig {
            passthrough_enabled: true,
        })
        .assemble(fs, nfs, source)
        .finish()
        .await
        .expect("assemble");

    // Pin a view at tip 150.
    let pinned = runtime.snapshot();
    assert_eq!(pinned.tip().await.expect("tip").height, h(150));

    // The live window advances.
    live_tip.store(160, Ordering::SeqCst);

    // The pinned view is unmoved — coherent as of when it was taken.
    assert_eq!(
        pinned.tip().await.expect("tip").height,
        h(150),
        "a pinned snapshot must not observe live advancement"
    );

    // A fresh snapshot sees the new tip...
    let later = runtime.snapshot();
    assert_eq!(later.tip().await.expect("tip").height, h(160));

    // ...and taking it did not retroactively move the first one.
    assert_eq!(pinned.tip().await.expect("tip").height, h(150));
}
