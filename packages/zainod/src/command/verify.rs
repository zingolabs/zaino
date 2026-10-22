//! `zainod verify`: every file each enabled index's manifest seals, against its page checksums
//!
//! - read-only: no index opened for writing, no directory created, no lock taken
//! - one check for every index (`DiskEngine::verify` with the index's schema)

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
};

use serde::Serialize;
use zaino_persistence::{
    fs::RealFs, DiskEngine, IndexKind, PersistenceEngine, StoreError, Verification,
};

use crate::config::{load_config, DaemonConfig, IndexConfig};
use zaino_nfs::schema;

/// - Disabled index = `None`
/// - `clean` = every sealed file present with every page intact
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Report {
    clean: bool,
    #[serde(flatten)]
    indexes: BTreeMap<&'static str, Option<Verification>>,
}

/// `Err` = indexes unreadable (exit 2)
#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifyError {
    #[error(transparent)]
    Config(#[from] crate::error::IndexerError),

    #[error("{index} index at {path}: {source}")]
    Index { index: &'static str, path: PathBuf, source: StoreError },

    #[error("rendering the report: {0}")]
    Render(#[from] serde_json::Error),
}

/// Verifies the indexes `config_path` enables (exit status 0 clean, 1 corrupt)
pub(crate) fn run(config_path: &Path) -> Result<ExitCode, VerifyError> {
    let report = verify(&load_config(config_path)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprint!("{}", report.summary());
    Ok(match report.clean {
        true => ExitCode::SUCCESS,
        false => ExitCode::FAILURE,
    })
}

fn verify(config: &DaemonConfig) -> Result<Report, VerifyError> {
    let engine = DiskEngine::new(RealFs::shared(), zaino_persistence::LsmConfig::default());
    let mut reports = BTreeMap::new();
    for kind in IndexKind::ALL {
        let index = kind.name();
        let report = config.enabled(kind).map(|IndexConfig { path, .. }| {
            engine
                .verify(&path, &schema(kind, config.network))
                .map_err(|source| VerifyError::Index { index, path, source })
        });
        reports.insert(index, report.transpose()?);
    }
    let clean = reports.values().flatten().all(Verification::is_clean);
    Ok(Report { clean, indexes: reports })
}

impl Report {
    /// One line per index, for a human on stderr
    fn summary(&self) -> String {
        let mut out = String::new();
        for (name, report) in &self.indexes {
            out += &match report {
                None => format!("{name}: disabled\n"),
                Some(report) => {
                    let units = &report.units;
                    let faults: usize = units
                        .iter()
                        .map(|unit| {
                            unit.bad_pages.len()
                                + usize::from(unit.lost)
                                + usize::from(unit.bad_sums)
                        })
                        .sum();
                    let orphaned: u64 = units.iter().map(|unit| unit.orphaned_bytes).sum();
                    format!(
                        "{name}: {} heights, {} files, {faults} faults, {orphaned} orphaned bytes\n",
                        report.heights,
                        units.len(),
                    )
                }
            };
        }
        out + match self.clean {
            true => "clean\n",
            false => "CORRUPTION FOUND\n",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use std::num::NonZeroUsize;
    use zaino_nfs::{writer, FeeWiring};
    use zaino_persistence::IndexKind;
    use zaino_source::testing::{all_at, h, p2pkh, MockChain};

    use zaino_sync::{FinalizedStream, Step};
    use zcash_protocol::consensus::NetworkType;

    /// Five indexes from one chain, scrubbed through the daemon's own config
    ///
    /// - clean = 0, flipped durable byte = its page named + 1, lost file = 1
    /// - disabled = skipped, no config = 2
    #[tokio::test]
    async fn verify_scrubs_every_sealed_file_and_exits_by_the_corruption_rule() {
        let root = tempfile::tempdir().expect("tempdir");
        // heights 0..4 (every upgrade from genesis): each a coinbase paying one p2pkh 500, plus
        // one sapling output (leaf = height + 1)
        let miner = p2pkh([0x11; 20]);
        let mut chain = MockChain::regtest()
            .upgrades(all_at(h(0)))
            .genesis_with(|b| b.coinbase(|c| c.pay(&miner, 500).sapling_output(1)));
        for leaf in 2..=4 {
            chain.mine(|b| b.coinbase(|c| c.pay(&miner, 500).sapling_output(leaf)));
        }
        let blocks = chain.blocks(chain.tip());

        let dir = |kind: IndexKind| root.path().join(kind.name());
        let (cb, bh, ts, ta) = (
            dir(IndexKind::CompactBlock),
            dir(IndexKind::BlockHash),
            dir(IndexKind::TreeState),
            dir(IndexKind::TransparentAddress),
        );
        // every index's writer wired as the daemon wires it, the chain sent as bulk
        let engine = DiskEngine::new(RealFs::shared(), zaino_persistence::LsmConfig::default());
        let batch = NonZeroUsize::new(1 << 20).expect("non-zero");
        let (mut block_sink, mut fees) = (FinalizedStream::new(), FeeWiring::new(Some(batch)));
        let mut loops = tokio::task::JoinSet::new();
        for kind in IndexKind::ALL {
            let store = engine.open(&dir(kind), &schema(kind, NetworkType::Main), batch);
            let (_, run) = writer(kind, store.expect("open"), &mut fees);
            loops.spawn(run(block_sink.subscribe(kind.name(), batch)));
        }
        for block in &blocks {
            block_sink.send(Step::Apply(Arc::clone(block))).await;
        }
        block_sink.shutdown();
        while let Some(indexed) = loops.join_next().await {
            indexed.expect("indexed through Shutdown");
        }

        let config_path = root.path().join("zainod.toml");
        std::fs::write(
            &config_path,
            format!(
                "[index.compact_block]\npath = {cb:?}\n\
                 [index.block_hash]\npath = {bh:?}\n\
                 [index.tree_state]\npath = {ts:?}\n\
                 [index.transparent_address]\npath = {ta:?}\n"
            ),
        )
        .expect("config");
        let config = load_config(&config_path).expect("load");

        use serde_json::{json, Value};
        let clean = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let indexes =
            ["compact_block", "utxo_set", "block_hash", "tree_state", "transparent_address"];
        // on disk = sealed bytes, then the zeroed reserve the appends grow into (not yet durable)
        let blocks_dat = &clean["compact_block"]["units"][0];
        let durable_bytes = blocks_dat["committed_bytes"].as_u64().expect("committed_bytes");
        let on_disk = std::fs::metadata(cb.join("blocks.dat")).expect("meta").len();
        assert!(on_disk > durable_bytes, "a reserve past the seal");
        let expected = json!({
            "name": "blocks.dat", "committed_bytes": durable_bytes,
            "orphaned_bytes": on_disk - durable_bytes,
            "lost": false, "bad_sums": false, "bad_pages": [],
        });
        let tree_units = clean["tree_state"]["units"].as_array().map(Vec::len);
        assert_eq!(clean["clean"], true, "{clean:#}");
        assert_eq!(indexes.map(|index| clean[index]["heights"].as_u64()), [Some(4); 5]);
        assert_eq!(*blocks_dat, expected);
        assert_eq!(tree_units, Some(1 + 3 * 33), "heights + 3 pools × (32 levels + subtrees)");
        assert_eq!(
            clean["header_chain"],
            Value::Null,
            "the header chain = memory, nothing to scrub"
        );
        assert_eq!(run(&config_path).expect("readable"), ExitCode::SUCCESS, "clean → exit 0");

        let heights = ts.join("heights.dat");
        let mut records = std::fs::read(&heights).expect("read");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("write");
        let corrupt = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let bad_pages = &corrupt["tree_state"]["units"][0]["bad_pages"];
        assert_eq!((&corrupt["clean"], bad_pages), (&json!(false), &json!([0])));
        let corrupt_exit = run(&config_path).expect("readable");
        assert_eq!(corrupt_exit, ExitCode::FAILURE, "any corruption → exit 1");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("restore");

        let segment = clean["block_hash"]["units"][0]["name"].as_str().expect("a durable segment");
        std::fs::remove_file(bh.join(segment)).expect("remove");
        let lost = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let lost_flag = &lost["block_hash"]["units"][0]["lost"];
        assert_eq!((&lost["clean"], lost_flag), (&json!(false), &json!(true)));

        // disabled = skipped and never created
        let absent = root.path().join("absent");
        let partial_path = root.path().join("partial.toml");
        std::fs::write(
            &partial_path,
            format!(
                "[index.compact_block]\nenabled = false\npath = {absent:?}\n\
                 [index.block_hash]\nenabled = false\npath = {absent:?}\n\
                 [index.tree_state]\npath = {ts:?}\n\
                 [index.transparent_address]\npath = {ta:?}\n"
            ),
        )
        .expect("config");
        let partial = serde_json::to_value(
            verify(&load_config(&partial_path).expect("load")).expect("verify"),
        )
        .expect("json");
        let disabled = ["compact_block", "utxo_set", "block_hash"].map(|i| &partial[i]);
        assert_eq!((&partial["clean"], disabled), (&json!(true), [&Value::Null; 3]));
        assert!(!absent.exists(), "verify created {}", absent.display());

        let unreadable = run(&root.path().join("missing.toml"));
        assert!(matches!(unreadable, Err(VerifyError::Config(_))), "{unreadable:?}");
    }
}
