//! Shared setup for the `sync-bench` (full pipeline) and `provision-bench`
//! (provisioner only) harness binaries: the source, logging, and reporting they
//! have in common. Each binary owns only what differs — sync-bench drives the
//! engine, provision-bench drains the provisioner's output.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zaino_indexer::FetchConcurrency;
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_source_zebra_readstate::ZebraReadStateAdapter;
use zebra_chain::parameters::Network;

/// A boxed error is enough for a benchmark binary — every step already carries a
/// typed cause, and the harness only reports the failure, it does not react to
/// its variant.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The resilient source both harnesses read compact blocks through.
pub type Source = Arc<ValidatorClient<ZebraReadStateAdapter>>;

/// The sync engine's default provisioner concurrency — the value a real indexer
/// runs with, so a bench that does not sweep the knob reflects it rather than a
/// swept value. Shared by both harnesses' `--concurrency` defaults.
pub fn default_concurrency() -> FetchConcurrency {
    FetchConcurrency::new(NonZeroUsize::new(16).expect("16 is non-zero"))
}

/// The networks the harnesses support, mapped to zebra's [`Network`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum NetworkArg {
    /// The Zcash main network.
    Mainnet,
    /// The Zcash test network.
    Testnet,
}

impl NetworkArg {
    /// The zebra [`Network`] this argument selects.
    pub fn to_zebra(self) -> Network {
        match self {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::new_default_testnet(),
        }
    }
}

/// Install the JSON tracing subscriber (→ stdout → Loki/Grafana on the cluster).
pub fn init_logging() {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Open the Zebra ReadState under `cache` (read-only) wrapped in the resilient
/// decorator, so a harness binds the resilient source ports rather than the raw
/// single-attempt adapter.
pub fn open_source(cache: &Path, network: &Network) -> Result<Source, BoxError> {
    let adapter = ZebraReadStateAdapter::open(cache, network)?;
    Ok(Arc::new(ValidatorClient::new(adapter, RetryPolicy::default())))
}

/// Report throughput over `blocks` as a human line and a structured event. `kind`
/// names the work timed ("indexed" for the full pipeline, "provisioned" for the
/// provisioner alone), so the two harnesses share one result schema.
pub fn report(kind: &str, blocks: u32, elapsed: Duration, concurrency: FetchConcurrency) {
    let seconds = elapsed.as_secs_f64();
    let per_second = f64::from(blocks) / seconds;
    println!(
        "{kind} {blocks} blocks in {seconds:.3}s = {per_second:.1} blocks/s \
         ({:.3} ms/block, concurrency {concurrency})",
        elapsed.as_secs_f64() * 1000.0 / f64::from(blocks),
    );
    tracing::info!(
        target: "sync_bench::result",
        kind,
        blocks,
        elapsed_ms = elapsed.as_millis(),
        blocks_per_second = per_second,
        concurrency = concurrency.get(),
        "bench window complete"
    );
}
