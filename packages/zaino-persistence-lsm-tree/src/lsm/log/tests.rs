use std::{num::NonZeroU32, path::PathBuf};

use super::*;
use crate::{fs::SimFs, lsm::Tuning, pages::PageError, testing::SPILL};

/// Each tier spends its own budget: tier 0's failures (and a landed tier-0 merge) never move
/// tier 1's count, so a tier that keeps failing is fatal after `max_merge_failures` transient
/// failures however the others fare
#[test]
fn each_tier_spends_its_own_merge_failure_budget() {
    let fs = SimFs::new();
    let dir = Path::new("/tiers");
    fs.create_dir_all(dir).expect("dir");
    let width = |n| NonZeroU32::new(n).expect("non-zero");
    let shape =
        Shape::of("tiers", width(8), width(8), false, Tuning::default(), SPILL).expect("shape");
    let lsm = LsmConfig {
        fanout: 2,
        ..LsmConfig::default()
    };
    let slots = Arc::new(Slots::new(1, u64::MAX));
    let labels = Labels::new("test", "tiers");
    let mut log = SegmentLog::open(fs, dir, &labels, shape, &[], &lsm, slots).expect("open");
    let fault = |log: &mut SegmentLog, tier| {
        let transient = SegmentError::Io(std::io::Error::other("EIO"));
        match log.merge_failed(tier, transient) {
            SegmentError::MergeFailed { fault, .. } => fault,
            other => panic!("not a merge failure: {other:?}"),
        }
    };
    for _ in 0..lsm.max_merge_failures {
        assert_eq!(
            fault(&mut log, 1),
            MergeFault::Transient,
            "tier 1 within budget"
        );
        assert_eq!(fault(&mut log, 0), MergeFault::Transient, "tier 0 its own");
        // tier 0's merge lands: its count alone resets
        log.failures.remove(&0);
    }
    assert_eq!(
        fault(&mut log, 1),
        MergeFault::Fatal,
        "tier 1 past its budget"
    );
    assert_eq!(
        fault(&mut log, 0),
        MergeFault::Transient,
        "tier 0 still within"
    );
}

/// Of several merges failing at once, the write reports the gravest: corrupt input over a fatal
/// failure over a transient one (a transient failure never hides one that must stop writes)
#[test]
fn the_gravest_of_several_merge_failures_is_kept() {
    let transient = SegmentError::Io(std::io::Error::other("EIO"));
    let fatal = SegmentError::Sync(std::io::Error::other("EIO"));
    let corrupt = SegmentError::Page(PageError::Corrupt {
        path: PathBuf::from("/x"),
        page: 0,
    });
    assert!(severity(&corrupt) > severity(&fatal));
    assert!(severity(&fatal) > severity(&transient));
}
