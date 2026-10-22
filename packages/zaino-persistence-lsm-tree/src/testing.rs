//! Shared by the crate's tests: the conformance namespaces opened on `SimFs`

use std::{
    num::NonZeroUsize,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use zaino_persistence::{
    conformance::{Subject, SCANNED, SPECS},
    Backend, BackendWriter, CommitError, Namespace, WriteOp,
};
use zaino_primitives::types::BlockRef;

use crate::{
    fs::SimFs,
    lsm::{LsmConfig, Tuning},
    LsmTreeBackend, LsmTreeConfig,
};

pub(crate) const ROOT: &str = "/idx";

/// Spill threshold the tests run with: small enough that every segment-writing test (crash
/// states included) goes through the scratch files
pub(crate) const SPILL: NonZeroUsize = NonZeroUsize::new(4096).expect("non-zero");

/// `scanned` keys share an 8-byte account prefix
pub(crate) const TUNING: [(Namespace, Tuning); 1] = [(
    SCANNED,
    Tuning {
        scope: 8,
        cache_writes: false,
    },
)];

/// `attempt` repeated until it yields (background merges land when a later write finds them
/// finished); panics naming `what` after 5 s
pub(crate) fn eventually<T>(what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(done) = attempt() {
            return done;
        }
        assert!(Instant::now() < deadline, "{what}: not within 5 s");
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn config(fanout: usize, write_buffer: NonZeroUsize) -> LsmTreeConfig<'static> {
    LsmTreeConfig {
        name: "conformance",
        format: 1,
        namespaces: &SPECS,
        tuning: &TUNING,
        lsm: LsmConfig {
            fanout,
            spill_bytes: SPILL,
            ..LsmConfig::default()
        },
        write_buffer,
    }
}

pub(crate) fn open(fs: &Arc<SimFs>, fanout: usize) -> LsmTreeBackend {
    LsmTreeBackend::open(
        fs.clone(),
        Path::new(ROOT),
        &config(fanout, NonZeroUsize::MAX),
    )
    .expect("open")
}

pub(crate) fn commit(
    backend: &LsmTreeBackend,
    (tip, ops): (BlockRef, Vec<WriteOp>),
) -> Result<(), CommitError> {
    backend.writer().expect("writer").commit(tip, ops)
}

/// The backend on `SimFs`: crash = `SimFs::power_loss`; every reopen verifies clean
pub(crate) struct SimDisk {
    pub(crate) fs: Arc<SimFs>,
    pub(crate) fanout: usize,
}

impl SimDisk {
    pub(crate) fn new(fanout: usize) -> Self {
        Self {
            fs: SimFs::new(),
            fanout,
        }
    }
}

impl Subject for SimDisk {
    type Backend = LsmTreeBackend;

    fn open(&self) -> LsmTreeBackend {
        open(&self.fs, self.fanout)
    }

    fn power_loss(&mut self) -> bool {
        self.fs = self.fs.power_loss();
        true
    }

    fn check(&self, backend: &LsmTreeBackend, just_opened: bool, label: &str) {
        if just_opened {
            let verified = backend.verify().expect("verify");
            assert!(verified.is_clean(), "{label}: {verified:?}");
        }
    }
}
