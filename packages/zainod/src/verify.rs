//! `zainod verify`: every file each enabled index's manifest seals, against its page checksums.
//!
//! - read-only: no index opened for writing, no directory created, no lock taken
//! - the same check for every index: bytes on disk = bytes sealed (what they mean was settled
//!   before they were written)

use std::{io, path::Path, path::PathBuf};

use serde::Serialize;
use zaino_persistence::pages::{scrub, CommittedFiles, Scrub};
use zcash_protocol::consensus::NetworkType;

use crate::config::{load_config, DaemonConfig, ZainoIndexConfig};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IndexReport {
    heights: u64,
    files: Vec<Scrub>,
}

impl IndexReport {
    fn is_clean(&self) -> bool {
        self.files.iter().all(Scrub::is_clean)
    }
}

/// Disabled index = `None`; `clean` = every sealed file present with every page intact
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Verification {
    clean: bool,
    compact_block: Option<IndexReport>,
    value_balance: Option<IndexReport>,
    block_hash: Option<IndexReport>,
    tree_state: Option<IndexReport>,
    transparent_address: Option<IndexReport>,
}

#[derive(Debug, thiserror::Error)]
enum VerifyError {
    #[error(transparent)]
    Config(#[from] crate::error::IndexerError),

    #[error("{index} index at {path}: {source}")]
    Index { index: &'static str, path: PathBuf, source: io::Error },

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

type Committed = fn(&Path, NetworkType) -> io::Result<CommittedFiles>;

fn verify(config: &DaemonConfig) -> Result<Verification, VerifyError> {
    let index = |name: &'static str, index: &ZainoIndexConfig, committed: Committed| {
        index
            .enabled
            .then(|| {
                let failed =
                    |source| VerifyError::Index { index: name, path: index.path.clone(), source };
                let committed = committed(&index.path, config.network).map_err(failed)?;
                let files = committed
                    .files
                    .iter()
                    .map(|(path, sealed)| scrub(&index.path, path, *sealed))
                    .collect::<io::Result<_>>()
                    .map_err(failed)?;
                let heights = committed.tip.map_or(0, |tip| u64::from(tip) + 1);
                Ok::<_, VerifyError>(IndexReport { heights, files })
            })
            .transpose()
    };

    let compact_block = index(
        "compact-block",
        &config.index.compact_block,
        zaino_index_compact_block::committed_files,
    )?;
    let value_balance = index(
        "value-balance",
        &config.index.value_balance,
        zaino_internal_value_balance::committed_files,
    )?;
    let block_hash = index(
        "block-hash",
        &config.index.block_hash,
        zaino_internal_block_hash_to_height::committed_files,
    )?;
    let tree_state =
        index("tree-state", &config.index.tree_state, zaino_index_tree_state::committed_files)?;
    let transparent_address = index(
        "transparent-address",
        &config.index.transparent_address,
        zaino_index_transparent_address::committed_files,
    )?;

    let clean = [&compact_block, &value_balance, &block_hash, &tree_state, &transparent_address]
        .into_iter()
        .flatten()
        .all(IndexReport::is_clean);
    Ok(Verification {
        clean,
        compact_block,
        value_balance,
        block_hash,
        tree_state,
        transparent_address,
    })
}

impl Verification {
    /// One line per index, for a human on stderr
    fn summary(&self) -> String {
        let mut out = String::new();
        for (name, report) in [
            ("compact_block", &self.compact_block),
            ("value_balance", &self.value_balance),
            ("block_hash", &self.block_hash),
            ("tree_state", &self.tree_state),
            ("transparent_address", &self.transparent_address),
        ] {
            out += &match report {
                None => format!("{name}: disabled\n"),
                Some(report) => {
                    let bad = |scrub: &Scrub| {
                        scrub.bad_pages.len()
                            + usize::from(scrub.lost)
                            + usize::from(scrub.bad_sums)
                    };
                    format!(
                        "{name}: {} heights, {} files, {} faults, {} orphaned bytes\n",
                        report.heights,
                        report.files.len(),
                        report.files.iter().map(bad).sum::<usize>(),
                        report.files.iter().map(|f| f.orphaned_bytes).sum::<u64>(),
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

    use zaino_index_compact_block::{CompactBlockIndexWriter, CompactBlockStore};
    use zaino_index_transparent_address::TransparentAddressIndexWriter;
    use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateStore};
    use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashStore};
    use zaino_internal_value_balance::ValueBalanceIndexWriter;
    use zaino_persistence::fs::RealFs;
    use zaino_primitives::types::{
        Block, BlockHeader, CompactCiphertext, SaplingData, SaplingOutput, Script, Transaction,
        TransactionId, TransparentData, TransparentOutput, Zatoshis,
    };
    use zaino_sync::{BlockWithFees, Derives as _, IndexWriter as _};

    /// All five indexes from one chain, scrubbed through the daemon's own config: clean = 0, a
    /// flipped committed byte = its page named + 1, a lost file = 1, disabled = skipped, no
    /// config = 2
    #[tokio::test]
    async fn verify_scrubs_every_sealed_file_and_exits_by_the_corruption_rule() {
        let root = tempfile::tempdir().expect("tempdir");
        let blocks: Vec<Arc<Block>> = (0u32..4)
            .map(|height| {
                let mut cmu = [0u8; 32];
                cmu[..4].copy_from_slice(&(height + 1).to_le_bytes());
                Arc::new(Block::new(
                    BlockHeader::for_tests(
                        height,
                        [height as u8 + 1; 32],
                        [height as u8; 32],
                        1_700_000_000 + height,
                    ),
                    vec![Transaction {
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
                    }],
                ))
            })
            .collect();

        let (cb, vb, bh, ts, ta) = (
            root.path().join("cb"),
            root.path().join("vb"),
            root.path().join("bh"),
            root.path().join("ts"),
            root.path().join("ta"),
        );
        let (fs, net) = (RealFs::shared(), NetworkType::Main);
        let mut compact = CompactBlockIndexWriter::new(
            CompactBlockStore::open(fs.clone(), &cb, net).expect("cb"),
        );
        let mut fees = ValueBalanceIndexWriter::open(fs.clone(), &vb, net).expect("vb writer");
        let mut paired = Vec::new();
        for block in &blocks {
            fees.deliver(std::slice::from_ref(block)).await.expect("vb deliver");
            let [block_fees] = <[_; 1]>::try_from(
                fees.derive(std::slice::from_ref(block)).await.expect("vb derive"),
            )
            .expect("one item per block");
            let (upstream, derived) = (Arc::clone(block), Arc::new(block_fees));
            paired.push(Arc::new(BlockWithFees { upstream, derived }));
        }
        zaino_sync::finalize_now(&mut fees, &blocks).await.expect("vb finalize");
        zaino_sync::finalize_now(&mut compact, &paired).await.expect("cb finalize");
        let mut hashes =
            BlockHashIndexWriter::new(BlockHashStore::open(fs.clone(), &bh, net).expect("bh"));
        zaino_sync::finalize_now(&mut hashes, &blocks).await.expect("bh finalize");
        let mut trees =
            TreeStateIndexWriter::new(TreeStateStore::open(fs.clone(), &ts, net).expect("ts"))
                .expect("ts writer");
        zaino_sync::finalize_now(&mut trees, &blocks).await.expect("ts finalize");
        let mut transparent = TransparentAddressIndexWriter::open(fs, &ta, net).expect("ta writer");
        zaino_sync::finalize_now(&mut transparent, &blocks).await.expect("ta finalize");
        drop((compact, fees, hashes, trees, transparent));

        let config_path = root.path().join("zainod.toml");
        std::fs::write(
            &config_path,
            format!(
                "[index.compact_block]\npath = {cb:?}\n\
                 [index.value_balance]\npath = {vb:?}\n\
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
            ["compact_block", "value_balance", "block_hash", "tree_state", "transparent_address"];
        let blocks_len = std::fs::metadata(cb.join("blocks.dat")).expect("meta").len();
        let blocks_dat = json!({
            "path": "blocks.dat", "committed_bytes": blocks_len, "orphaned_bytes": 0,
            "lost": false, "bad_sums": false, "bad_pages": [],
        });
        let tree_files = clean["tree_state"]["files"].as_array().map(Vec::len);
        assert_eq!(clean["clean"], true, "{clean:#}");
        assert_eq!(indexes.map(|index| clean[index]["heights"].as_u64()), [Some(4); 5]);
        assert_eq!(clean["compact_block"]["files"][0], blocks_dat);
        assert_eq!(tree_files, Some(1 + 3 * 33), "heights + 3 pools × (32 levels + subtrees)");
        assert_eq!(run(&config_path), 0, "clean → exit 0");

        let heights = ts.join("heights.idx");
        let mut records = std::fs::read(&heights).expect("read");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("write");
        let corrupt = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let bad_pages = &corrupt["tree_state"]["files"][0]["bad_pages"];
        assert_eq!((&corrupt["clean"], bad_pages), (&json!(false), &json!([0])));
        assert_eq!(run(&config_path), 1, "any corruption → exit 1");
        records[3] ^= 0x01;
        std::fs::write(&heights, &records).expect("restore");

        let segment =
            clean["block_hash"]["files"][0]["path"].as_str().expect("a committed segment");
        std::fs::remove_file(bh.join(segment)).expect("remove");
        let lost = serde_json::to_value(verify(&config).expect("verify")).expect("json");
        let lost_flag = &lost["block_hash"]["files"][0]["lost"];
        assert_eq!((&lost["clean"], lost_flag), (&json!(false), &json!(true)));

        // disabled = skipped and never created
        let absent = root.path().join("absent");
        let partial_path = root.path().join("partial.toml");
        std::fs::write(
            &partial_path,
            format!(
                "[index.compact_block]\nenabled = false\npath = {absent:?}\n\
                 [index.value_balance]\nenabled = false\npath = {absent:?}\n\
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
        let disabled = (&partial["compact_block"], &partial["block_hash"]);
        assert_eq!((&partial["clean"], disabled), (&json!(true), (&Value::Null, &Value::Null)));
        assert!(!absent.exists(), "verify created {}", absent.display());

        assert_eq!(run(&root.path().join("missing.toml")), 2, "unreadable → exit 2");
    }
}
