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

/// Spill threshold the tests run with: 64 bytes, so a segment of a few dozen rows (the crash
/// workloads' included) streams its filter through a scratch file
pub(crate) const SPILL: NonZeroUsize = NonZeroUsize::new(64).expect("non-zero");

/// Pool threads the tests run with: real work-stealing, without every parallel test's backends
/// claiming all cores
pub(crate) const THREADS: NonZeroUsize = NonZeroUsize::new(2).expect("non-zero");

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
        threads: Some(THREADS),
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
///
/// - `write_buffer` below `MAX`: the backend writes on its own once its buffer fills
pub(crate) struct SimDisk {
    pub(crate) fs: Arc<SimFs>,
    pub(crate) fanout: usize,
    pub(crate) write_buffer: NonZeroUsize,
    pub(crate) threads: NonZeroUsize,
}

impl SimDisk {
    /// Written only by `flush`
    pub(crate) fn new(fanout: usize) -> Self {
        Self::writing_at(fanout, NonZeroUsize::MAX)
    }

    /// Written whenever the buffer reaches `write_buffer` too
    pub(crate) fn writing_at(fanout: usize, write_buffer: NonZeroUsize) -> Self {
        Self {
            fs: SimFs::new(),
            fanout,
            write_buffer,
            threads: THREADS,
        }
    }

    /// Its pool on one thread (a one-core host)
    pub(crate) fn on_one_thread(self) -> Self {
        Self {
            threads: NonZeroUsize::MIN,
            ..self
        }
    }
}

impl Subject for SimDisk {
    type Backend = LsmTreeBackend;

    fn open(&self) -> LsmTreeBackend {
        let config = LsmTreeConfig {
            threads: Some(self.threads),
            ..config(self.fanout, self.write_buffer)
        };
        LsmTreeBackend::open(self.fs.clone(), Path::new(ROOT), &config).expect("open")
    }

    fn writes_on_its_own(&self) -> bool {
        self.write_buffer != NonZeroUsize::MAX
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
