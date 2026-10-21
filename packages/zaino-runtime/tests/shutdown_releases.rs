//! A teardown is finished when the components have let go of what they held.
//!
//! The runtime re-boots after an escalation, binding the same ports again, so a
//! shutdown that merely *asks* components to stop is indistinguishable from one
//! that worked until the next boot fails to bind. These tests pin the two
//! properties that make the difference: the components' own `stop` is awaited,
//! and it happens in reverse boot order.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;
use zaino_component::{CancellationToken, ComponentName, Lifecycle, RunReporter};
use zaino_runtime::{OrchestraBuilder, RunComponent, RunLoop};

#[derive(Debug, thiserror::Error)]
#[error("serve failed")]
struct ServeError;

/// Holds a bound socket for as long as its run loop is alive, like a real
/// server. The socket is dropped when `run` returns.
struct Listening {
    addr: std::net::SocketAddr,
}

impl RunLoop for Listening {
    type Error = ServeError;
    const LABEL: &'static str = "serve loop";
    const RUNNING: Lifecycle = Lifecycle::Spawning;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), ServeError> {
        let listener = TcpListener::bind(self.addr).await.map_err(|_| ServeError)?;
        reporter.ready();
        cancel.cancelled().await;
        drop(listener);
        Ok(())
    }
}

/// A shutdown that has returned has freed the ports its components held, so the
/// next boot can bind them.
#[tokio::test]
async fn a_returned_shutdown_has_freed_the_served_port() {
    // Port 0 lets the OS pick a free one; re-binding *that* address is the test.
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let server = RunComponent::new(ComponentName("server"), Listening { addr });
    let orchestra = OrchestraBuilder::new()
        .boot(server)
        .await
        .expect("boot server")
        .build();

    orchestra.shutdown().await;

    // No retry loop and no sleep: if this needs either, the shutdown did not
    // wait, which is the defect.
    TcpListener::bind(addr)
        .await
        .expect("the port is free the moment shutdown returns");
}

/// Records the order in which components were stopped.
struct Recording {
    name: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl RunLoop for Recording {
    type Error = ServeError;
    const LABEL: &'static str = "run loop";
    const RUNNING: Lifecycle = Lifecycle::Syncing;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), ServeError> {
        reporter.ready();
        cancel.cancelled().await;
        self.order
            .lock()
            .expect("order lock")
            .push(self.name);
        Ok(())
    }
}

/// Components are stopped in reverse boot order, so one is never torn down
/// while something booted on top of it is still serving.
#[tokio::test]
async fn components_stop_in_reverse_boot_order() {
    let order = Arc::new(Mutex::new(Vec::new()));

    let mut builder = OrchestraBuilder::new();
    for name in ["store", "indexer", "server"] {
        builder = builder
            .boot(RunComponent::new(
                ComponentName(name),
                Recording {
                    name,
                    order: Arc::clone(&order),
                },
            ))
            .await
            .expect("boot");
    }
    let orchestra = builder.build();

    orchestra.shutdown().await;

    assert_eq!(
        *order.lock().expect("order lock"),
        vec!["server", "indexer", "store"],
        "a component is stopped only after everything booted on top of it"
    );
}

/// One component failing to stop must not strand the rest.
#[tokio::test]
async fn a_component_that_fails_to_stop_does_not_strand_the_others() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let orchestra = OrchestraBuilder::new()
        .boot(RunComponent::new(
            ComponentName("store"),
            Recording {
                name: "store",
                order: Arc::clone(&order),
            },
        ))
        .await
        .expect("boot store")
        .build();

    tokio::time::timeout(Duration::from_secs(5), orchestra.shutdown())
        .await
        .expect("shutdown does not hang");

    assert_eq!(*order.lock().expect("order lock"), vec!["store"]);
    assert_eq!(orchestra.statuses().len(), 1);
}

/// A run loop that fails once it is Ready, to drive an escalation.
struct FailsAfterReady;

impl RunLoop for FailsAfterReady {
    type Error = ServeError;
    const LABEL: &'static str = "serve loop";
    const RUNNING: Lifecycle = Lifecycle::Spawning;

    async fn run(
        self: Arc<Self>,
        _cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), ServeError> {
        reporter.ready();
        Err(ServeError)
    }
}

/// An escalation still releases the *other* components' resources.
///
/// The escalated component is stepped over to keep its `Critical` reason, which
/// must not become an excuse to skip the teardown it triggered — the port its
/// neighbour holds has to be free for the re-boot either way.
#[tokio::test]
async fn an_escalation_still_stops_the_other_components() {
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let orchestra = OrchestraBuilder::new()
        .boot(RunComponent::new(ComponentName("server"), Listening { addr }))
        .await
        .expect("boot server")
        .boot(RunComponent::new(ComponentName("doomed"), FailsAfterReady))
        .await
        .expect("boot doomed")
        .build();

    let outcome = tokio::time::timeout(Duration::from_secs(5), orchestra.run())
        .await
        .expect("orchestra reached a decision");
    assert!(
        matches!(outcome, zaino_runtime::RuntimeOutcome::Fatal { .. }),
        "the failing component is fatal under today's policy"
    );

    TcpListener::bind(addr)
        .await
        .expect("the neighbour's port is free once the escalation teardown returns");
}
