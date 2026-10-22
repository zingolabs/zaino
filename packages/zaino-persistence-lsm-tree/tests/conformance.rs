//! The port's conformance suite against the backend on the real filesystem, through its public
//! API only (crash states and merges' internals: the crate's own tests, on `SimFs`)

use std::{num::NonZeroUsize, path::PathBuf};

use proptest::prelude::*;
use tempfile::TempDir;
use zaino_persistence::conformance::{self, Subject, SCANNED, SPECS};
use zaino_persistence_lsm_tree::{LsmConfig, LsmTreeBackend, LsmTreeConfig, Tuning};

/// A temporary directory reopened as the conformance index, merging every two segments
struct RealDir {
    _root: TempDir,
    path: PathBuf,
}

impl RealDir {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("idx");
        Self { _root: root, path }
    }
}

impl Subject for RealDir {
    type Backend = LsmTreeBackend;

    fn open(&self) -> LsmTreeBackend {
        let tuning = [(
            SCANNED,
            Tuning {
                scope: 8,
                cache_writes: false,
            },
        )];
        let config = LsmTreeConfig {
            name: "conformance",
            format: 1,
            namespaces: &SPECS,
            tuning: &tuning,
            lsm: LsmConfig {
                fanout: 2,
                spill_bytes: NonZeroUsize::new(4096).expect("non-zero"),
                ..LsmConfig::default()
            },
            write_buffer: NonZeroUsize::MAX,
        };
        LsmTreeBackend::open_real(&self.path, &config).expect("open")
    }
}

#[test]
fn the_backend_keeps_the_port_contract_on_a_real_filesystem() {
    conformance::contract(RealDir::new());
}

proptest! {
    // a real directory per case: fewer cases than the crate's `SimFs` run
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    #[test]
    fn random_histories_answer_like_a_btreemap_on_a_real_filesystem(
        steps in conformance::steps(),
    ) {
        conformance::history(RealDir::new(), &steps);
    }
}
