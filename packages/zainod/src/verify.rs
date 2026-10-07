//! `zainod verify`: every file each enabled index's manifest seals, against its page checksums.
//!
//! - read-only: no index opened for writing, no directory created, no lock taken
//! - one check for every index (`DiskEngine::verify` with the index's schema)

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use zaino_persistence::{
    fs::RealFs, DiskEngine, IndexKind, PersistenceEngine, StoreError, Verification,
};

use crate::config::{load_config, DaemonConfig, IndexConfig};

/// Disabled index = `None`; `clean` = every sealed file present with every page intact
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Report {
    clean: bool,
    #[serde(flatten)]
    indexes: BTreeMap<&'static str, Option<Verification>>,
}

#[derive(Debug, thiserror::Error)]
enum VerifyError {
    #[error(transparent)]
    Config(#[from] crate::error::IndexerError),

    #[error("{index} index at {path}: {source}")]
    Index { index: &'static str, path: PathBuf, source: StoreError },

    #[error("rendering the report: {0}")]
    Render(#[from] serde_json::Error),
}

/// Verifies the indexes `config_path` enables; exit status 0 clean, 1 corrupt, 2 unreadable
pub fn run(config_path: &Path) -> i32 {
    let outcome = load_config(config_path)
        .map_err(VerifyError::from)
        .and_then(|config| verify(&config))
        .and_then(|report| Ok((serde_json::to_string_pretty(&report)?, report)));

    match outcome {
        Ok((json, report)) => {
            println!("{json}");
            eprint!("{}", report.summary());
            match report.clean {
                true => 0,
                false => 1,
            }
        }
        Err(error) => {
            eprintln!("zainod verify: {error}");
            2
        }
    }
}

fn verify(config: &DaemonConfig) -> Result<Report, VerifyError> {
    let indexes = [
        IndexKind::CompactBlock,
        IndexKind::ValueBalance,
        IndexKind::BlockHash,
        IndexKind::TreeState,
        IndexKind::TransparentAddress,
        IndexKind::HeaderChain,
    ];

    let engine = DiskEngine::new(RealFs::shared());
    let mut reports = BTreeMap::new();
    for kind in indexes {
        let index = kind.name();
        let report = config.enabled(kind).map(|IndexConfig { path, .. }| {
            engine
                .verify(&path, &zaino_nfs::schema(kind, config.network))
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
    use zaino_index_compact_block::CompactBlockIndexWriter;
    use zaino_index_transparent_address::TransparentAddressIndexWriter;
    use zaino_index_tree_state::TreeStateIndexWriter;
    use zaino_internal_block_hash_to_height::BlockHashIndexWriter;
    use zaino_internal_value_balance::ValueBalanceIndexWriter;
    use zaino_persistence::fs::RealFs;
    use zaino_persistence::Schema;
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{
        Block, CompactCiphertext, SaplingData, SaplingOutput, Script, Transaction, TransactionId,
        TransparentData, TransparentOutput, Zatoshis,
    };

    use zaino_sync::{FeeSink, Final, IndexerDataSink, Step};
    use zcash_protocol::consensus::NetworkType;

    /// All five indexes from one chain, scrubbed through the daemon's own config: clean = 0, a
    /// flipped committed byte = its page named + 1, a lost file = 1, disabled = skipped, no
    /// config = 2
    #[tokio::test]
    async fn verify_scrubs_every_sealed_file_and_exits_by_the_corruption_rule() {
        let root = tempfile::tempdir().expect("tempdir");
        // heights 0..4: each a coinbase paying one p2pkh, plus one sapling output
        let coinbase = |height: u32| {
            let mut cmu = [0u8; 32];
            cmu[..4].copy_from_slice(&(height + 1).to_le_bytes());
            Transaction {
                txid: TransactionId::from([0xa0 + height as u8; 32]),
                transparent: TransparentData {
                    coinbase: true,
                    inputs: Vec::new(),
                    outputs: vec![TransparentOutput {
                        value: Zatoshis::new(500).expect("in supply"),
                        script: Script::new(
                            [&[0x76, 0xa9, 0x14][..], &[0x11; 20], &[0x88, 0xac]].concat(),
                        ),
                    }],
                },
                sprout: Default::default(),
                sapling: SaplingData {
                    outputs: vec![SaplingOutput {
                        cmu: cmu.into(),
                        ephemeral_key: [2u8; 32].into(),
                        enc_ciphertext: [3u8; CompactCiphertext::LENGTH].into(),
                    }],
                    ..Default::default()
                },
                orchard: Default::default(),
                ironwood: Default::default(),
            }
        };
        let mut chain = Chain::with_genesis(vec![coinbase(0)]);
        let tip =
            (1..4).fold(chain.genesis(), |tip, h| chain.mine_with(tip.hash, vec![coinbase(h)]));
        let blocks: Vec<Arc<Block>> = chain.path(tip.hash).into_iter().map(Arc::new).collect();

        let (cb, vb, bh, ts, ta) = (
            root.path().join("cb"),
            root.path().join(IndexKind::ValueBalance.name()),
            root.path().join("bh"),
            root.path().join("ts"),
            root.path().join("ta"),
        );
        // the five index loops wired as the daemon wires them, the chain sent as bulk
        let (engine, net) = (DiskEngine::new(RealFs::shared()), NetworkType::Main);
        let batch = NonZeroUsize::new(1 << 20).expect("non-zero");
        let (mut block_sink, mut fee_sink) = (IndexerDataSink::new("final"), FeeSink::new("fees"));
        let mut subscribe = |index: IndexKind| block_sink.subscribe(index.name(), batch);
        let (compact_blocks, fee_blocks) =
            (subscribe(IndexKind::CompactBlock), subscribe(IndexKind::ValueBalance));
        let hash_blocks = subscribe(IndexKind::BlockHash);
        let tree_blocks = subscribe(IndexKind::TreeState);
        let transparent_blocks = subscribe(IndexKind::TransparentAddress);
        let compact_fees = fee_sink.subscribe(IndexKind::CompactBlock.name(), batch);
        let open = |path: &Path, schema: Schema| engine.open(path, &schema).expect("open");
        let store = open(&cb, zaino_index_compact_block::schema(net));
        let compact = CompactBlockIndexWriter::new(store, batch);
        let store = open(&vb, zaino_internal_value_balance::schema(net));
        let fees = ValueBalanceIndexWriter::new(store, batch);
        let store = open(&bh, zaino_internal_block_hash_to_height::schema(net));
        let hashes = BlockHashIndexWriter::new(store, batch);
        let store = open(&ts, zaino_index_tree_state::schema(net));
        let trees = TreeStateIndexWriter::new(store, batch);
        let store = open(&ta, zaino_index_transparent_address::schema(net));
        let transparent = TransparentAddressIndexWriter::new(store, batch);

        let mut loops = tokio::task::JoinSet::new();
        loops.spawn(compact.run(compact_blocks, compact_fees));
        loops.spawn(fees.run(fee_blocks, fee_sink));
        loops.spawn(hashes.run(hash_blocks));
        loops.spawn(trees.run(tree_blocks));
        loops.spawn(transparent.run(transparent_blocks));
        for block in &blocks {
            let (height, block) = (block.header().height, Arc::clone(block));
            let data = Arc::new(Final { block, folds: None });
            block_sink.send(Step::Apply { height, data }).await;
        }
        block_sink.shutdown();
        while let Some(indexed) = loops.join_next().await {
            indexed.expect("indexed through Shutdown");
        }

        // header chain never synced here: an absent directory verifies as empty, never created
        let hc = root.path().join("hc");
        let config_path = root.path().join("zainod.toml");
        std::fs::write(
            &config_path,
            format!(
                "[index.compact_block]\npath = {cb:?}\n\
                 [index.block_hash]\npath = {bh:?}\n\
                 [index.tree_state]\npath = {ts:?}\n\
                 [index.transparent_address]\npath = {ta:?}\n\
                 [index.header_chain]\npath = {hc:?}\n"
            ),
        )
        .expect("config");
        let config = load_config(&config_path).expect("load");

        use serde_json::{json, Value};
        let clean = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let indexes =
            ["compact_block", "value_balance", "block_hash", "tree_state", "transparent_address"];
        // on disk = the sealed bytes, then the zeroed reserve the appends grow into (uncommitted)
        let blocks_dat = &clean["compact_block"]["units"][0];
        let committed = blocks_dat["committed_bytes"].as_u64().expect("committed bytes");
        let on_disk = std::fs::metadata(cb.join("blocks.dat")).expect("meta").len();
        assert!(on_disk > committed, "a reserve past the seal");
        let expected = json!({
            "name": "blocks.dat", "committed_bytes": committed,
            "orphaned_bytes": on_disk - committed,
            "lost": false, "bad_sums": false, "bad_pages": [],
        });
        let tree_units = clean["tree_state"]["units"].as_array().map(Vec::len);
        assert_eq!(clean["clean"], true, "{clean:#}");
        assert_eq!(indexes.map(|index| clean[index]["heights"].as_u64()), [Some(4); 5]);
        assert_eq!(*blocks_dat, expected);
        assert_eq!(tree_units, Some(1 + 3 * 33), "heights + 3 pools × (32 levels + subtrees)");
        assert_eq!(clean["header_chain"], json!({ "heights": 0, "units": [] }));
        assert!(!hc.exists(), "verify created {}", hc.display());
        assert_eq!(run(&config_path), 0, "clean → exit 0");

        let heights = ts.join("heights.dat");
        let mut records = std::fs::read(&heights).expect("read");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("write");
        let corrupt = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let bad_pages = &corrupt["tree_state"]["units"][0]["bad_pages"];
        assert_eq!((&corrupt["clean"], bad_pages), (&json!(false), &json!([0])));
        assert_eq!(run(&config_path), 1, "any corruption → exit 1");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("restore");

        let segment =
            clean["block_hash"]["units"][0]["name"].as_str().expect("a committed segment");
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
                 [index.transparent_address]\npath = {ta:?}\n\
                 [index.header_chain]\npath = {hc:?}\n"
            ),
        )
        .expect("config");
        let partial = serde_json::to_value(
            verify(&load_config(&partial_path).expect("load")).expect("verify"),
        )
        .expect("json");
        let disabled = ["compact_block", "value_balance", "block_hash"].map(|i| &partial[i]);
        assert_eq!((&partial["clean"], disabled), (&json!(true), [&Value::Null; 3]));
        assert!(!absent.exists(), "verify created {}", absent.display());

        assert_eq!(run(&root.path().join("missing.toml")), 2, "unreadable → exit 2");
    }
}
