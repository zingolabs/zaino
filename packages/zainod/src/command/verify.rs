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
use zaino_persistence::{fs::RealFs, DiskEngine, PersistenceEngine, StoreError, Verification};
use zaino_sync::IndexKind;

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
}

/// Verifies the indexes `config_path` enables (exit status 0 clean, 1 corrupt, 2 unreadable)
pub(crate) fn run(config_path: &Path) -> ExitCode {
    crate::exit(scrub(config_path), 2)
}

fn scrub(config_path: &Path) -> Result<ExitCode, VerifyError> {
    let report = verify(&load_config(config_path)?)?;
    let rendered = serde_json::to_string_pretty(&report).expect("verify Report serializes to JSON");
    println!("{rendered}");
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
        let name = kind.name();
        let report = config.indexes.get(kind).map(|IndexConfig { path, .. }| {
            engine
                .verify(path, &schema(kind, config.network))
                .map_err(|source| VerifyError::Index { index: name, path: path.clone(), source })
        });
        reports.insert(name, report.transpose()?);
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

    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use serde_json::{json, Value};
    use zaino_nfs::{writer, FeeWiring};
    use zaino_source::testing::{all_at, h, p2pkh, MockChain};
    use zaino_sync::{FinalizedStream, Step};
    use zcash_protocol::consensus::NetworkType;

    /// Every index under `root/<name>`, its writer wired as the daemon wires it, fed a 4-block
    /// regtest chain (heights 0..=3, each coinbase = one p2pkh 500 + one sapling output)
    async fn index_four_blocks(root: &Path) {
        let miner = p2pkh([0x11; 20]);
        let mut chain = MockChain::regtest()
            .upgrades(all_at(h(0)))
            .genesis_with(|b| b.coinbase(|c| c.pay(&miner, 500).sapling_output(1)));
        for leaf in 2..=4 {
            chain.mine(|b| b.coinbase(|c| c.pay(&miner, 500).sapling_output(leaf)));
        }
        let engine = DiskEngine::new(RealFs::shared(), zaino_persistence::LsmConfig::default());
        let batch = NonZeroUsize::new(1 << 20).expect("non-zero");
        let (mut block_sink, mut fees) = (FinalizedStream::new(), FeeWiring::new(Some(batch)));
        let mut loops = tokio::task::JoinSet::new();
        for kind in IndexKind::ALL {
            let schema = schema(kind, NetworkType::Main);
            let store = engine.open(&root.join(kind.name()), &schema, batch).expect("open");
            let (_, run) = writer(kind, store, &mut fees);
            loops.spawn(run(block_sink.subscribe(kind.name(), batch)));
        }
        for block in chain.blocks(chain.tip()) {
            block_sink.send(Step::Apply(Arc::clone(&block))).await;
        }
        block_sink.shutdown();
        while let Some(indexed) = loops.join_next().await {
            indexed.expect("indexed through Shutdown");
        }
    }

    /// Config enabling every index at `root/<name>` (utxo_set beside compact_block)
    fn every_index_config(root: &Path) -> PathBuf {
        let dir = |kind: IndexKind| root.join(kind.name());
        let toml = format!(
            "[index.compact_block]\npath = {:?}\n[index.block_hash]\npath = {:?}\n\
             [index.tree_state]\npath = {:?}\n[index.transparent_address]\npath = {:?}\n\
             [index.txid_location]\npath = {:?}\n",
            dir(IndexKind::CompactBlock),
            dir(IndexKind::BlockHash),
            dir(IndexKind::TreeState),
            dir(IndexKind::TransparentAddress),
            dir(IndexKind::TxidLocation),
        );
        let path = root.join("zainod.toml");
        std::fs::write(&path, toml).expect("config");
        path
    }

    /// `index`'s unit `file`, as the report renders it
    fn unit(report: &Report, index: &str, file: &str) -> Value {
        let rendered = serde_json::to_value(report).expect("json");
        let units = rendered[index]["units"].as_array().cloned().unwrap_or_default();
        let found = units.into_iter().find(|unit| unit["name"] == file);
        found.unwrap_or_else(|| panic!("{index} has no unit {file}: {rendered:#}"))
    }

    #[tokio::test]
    async fn a_clean_index_set_reports_every_sealed_file_and_exits_0() {
        let root = tempfile::tempdir().expect("tempdir");
        index_four_blocks(root.path()).await;
        let config_path = every_index_config(root.path());

        let report = verify(&load_config(&config_path).expect("load")).expect("verify");

        let heights: Vec<_> =
            report.indexes.iter().map(|(name, v)| (*name, v.as_ref().map(|v| v.heights))).collect();
        let every = IndexKind::ALL.map(|kind| (kind.name(), Some(4)));
        let mut every = every.to_vec();
        every.sort();
        assert_eq!(heights, every, "every index (no header chain: memory only): 4 heights durable");
        assert!(report.clean, "{report:#?}");
        // on disk = sealed bytes, then the zeroed reserve the appends grow into (not yet durable)
        let blocks_dat = unit(&report, "compact_block", "blocks.dat");
        let sealed = blocks_dat["committed_bytes"].as_u64().expect("committed_bytes");
        let on_disk = std::fs::metadata(root.path().join("compact_block/blocks.dat"));
        let on_disk = on_disk.expect("blocks.dat").len();
        let expected = json!({
            "name": "blocks.dat", "committed_bytes": sealed, "orphaned_bytes": on_disk - sealed,
            "lost": false, "bad_sums": false, "bad_pages": [],
        });
        assert_eq!(
            blocks_dat, expected,
            "blocks.dat: {on_disk} B on disk, a reserve past the seal"
        );
        let tree_state = report.indexes["tree_state"].as_ref().map(|v| v.units.len());
        assert_eq!(
            tree_state,
            Some(1 + 3 * 33),
            "tree_state files: heights + 3 pools × (32 levels + subtrees)"
        );

        assert_eq!(run(&config_path), ExitCode::SUCCESS, "clean → exit 0");
    }

    #[tokio::test]
    async fn a_flipped_durable_byte_names_its_page_and_exits_1() {
        // any sealed byte of heights.dat's first page (4 heights fit one page)
        const IN_PAGE_0: usize = 3;
        let root = tempfile::tempdir().expect("tempdir");
        index_four_blocks(root.path()).await;
        let config_path = every_index_config(root.path());
        let heights = root.path().join("tree_state/heights.dat");
        let mut sealed = std::fs::read(&heights).expect("read");
        sealed[IN_PAGE_0] ^= 0x01;
        std::fs::write(&heights, &sealed).expect("write");

        let report = verify(&load_config(&config_path).expect("load")).expect("verify");

        let heights_dat = unit(&report, "tree_state", "heights.dat");
        assert_eq!(heights_dat["bad_pages"], json!([0]), "page 0 named: {heights_dat}");
        assert!(!report.clean, "a bad page = not clean");
        assert_eq!(run(&config_path), ExitCode::FAILURE, "any corruption → exit 1");
    }

    #[tokio::test]
    async fn a_lost_sealed_file_is_reported_lost_and_exits_1() {
        let root = tempfile::tempdir().expect("tempdir");
        index_four_blocks(root.path()).await;
        let config_path = every_index_config(root.path());
        let config = load_config(&config_path).expect("load");
        let rendered = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let segment = rendered["block_hash"]["units"][0]["name"].as_str().expect("a segment");
        std::fs::remove_file(root.path().join("block_hash").join(segment)).expect("remove");

        let report = verify(&config).expect("verify");

        assert_eq!(unit(&report, "block_hash", segment)["lost"], true, "{segment} lost");
        assert!(!report.clean, "a lost file = not clean");
        assert_eq!(run(&config_path), ExitCode::FAILURE, "any corruption → exit 1");
    }

    #[tokio::test]
    async fn a_disabled_index_is_skipped_and_never_created() {
        let root = tempfile::tempdir().expect("tempdir");
        index_four_blocks(root.path()).await;
        let absent = root.path().join("absent");
        let (ts, ta) = (root.path().join("tree_state"), root.path().join("transparent_address"));
        let config_path = root.path().join("partial.toml");
        let toml = format!(
            "[index.compact_block]\nenabled = false\npath = {absent:?}\n\
             [index.block_hash]\nenabled = false\npath = {absent:?}\n\
             [index.txid_location]\nenabled = false\npath = {absent:?}\n\
             [index.tree_state]\npath = {ts:?}\n[index.transparent_address]\npath = {ta:?}\n"
        );
        std::fs::write(&config_path, toml).expect("config");

        let report = verify(&load_config(&config_path).expect("load")).expect("verify");

        let enabled: Vec<_> = report.indexes.iter().map(|(name, v)| (*name, v.is_some())).collect();
        let expected = [
            ("block_hash", false),
            ("compact_block", false),
            ("transparent_address", true),
            ("tree_state", true),
            ("txid_location", false),
            ("utxo_set", false),
        ];
        assert_eq!(enabled, expected, "(index, verified): utxo_set off with compact_block");
        assert!(report.clean, "{report:#?}");
        assert!(!absent.exists(), "verify created {}", absent.display());
    }

    #[test]
    fn an_unreadable_config_exits_2_naming_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let missing = root.path().join("missing.toml");

        let refused = scrub(&missing).map_err(|error| error.to_string());

        assert!(
            refused.as_ref().is_err_and(|refused| refused.contains("missing.toml")),
            "refusal names the config: {refused:?}"
        );
        assert_eq!(run(&missing), ExitCode::from(2), "unreadable → exit 2");
    }
}
