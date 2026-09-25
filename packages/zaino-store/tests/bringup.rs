//! EXPLORATORY: a minimal real bringup — validator → indexer → store — through
//! the Orchestra, proving the store reader (via [`StoreComponent`]) joins the
//! supervised component set and boots in dependency order after the writer.
//!
//! Drivers are the simplest that are still *real components*: the validator is
//! confirmed through a stub reachability probe, and the indexer runs a no-op
//! sync driver that reaches the tip immediately and then follows until
//! cancelled. The point is the composition and boot order, not real syncing.

use std::sync::Arc;

use zaino_component::{
    CancellationToken, ComponentName, Lifecycle, ReachabilityProbe, RunLoop, RunReporter,
};
use zaino_indexes::sets::current_zaino::CurrentZaino;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
use zaino_store::{StoreComponent, StoreReader};

/// A validator that is reachable.
struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

/// A sync driver that reaches the tip at once, then idles until cancelled.
struct NoOpDriver;
impl RunLoop for NoOpDriver {
    type Error = std::convert::Infallible;
    const LABEL: &'static str = "run loop";
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), Self::Error> {
        reporter.ready();
        cancel.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn validator_indexer_store_boot_in_order() {
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");
    let indexer = IndexerComponent::new(ComponentName("indexer"), NoOpDriver);
    let reader = StoreReader::<_, CurrentZaino>::new(Arc::new(InMemoryBackend::new()));
    let store = StoreComponent::new(ComponentName("store"), reader);

    // Root first (observed), then the writer, then the reader that reads behind
    // it. Each reaches `Ready` before the next is booted.
    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots")
        .boot(store)
        .await
        .expect("store boots")
        .build();

    let statuses = orchestra.statuses();

    let names: Vec<_> = statuses.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec![
            ComponentName("validator"),
            ComponentName("indexer"),
            ComponentName("store"),
        ],
        "components appear in boot order"
    );

    for status in &statuses {
        assert_eq!(
            status.lifecycle,
            Lifecycle::Ready,
            "{} should be Ready after bringup",
            status.name
        );
    }
}
