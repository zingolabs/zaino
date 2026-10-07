//! `Tiered` over a `DiskStore`: staged and applied blocks against `Model`s, each precondition
//! refused before any state moves, each [`Tiered::check`] invariant fired by hand

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};

use zcash_protocol::consensus::NetworkType;

use super::*;
use crate::{
    conformance::{panic_message, probed_key, schema, Model, PROBED},
    fs::SimFs,
    manifest::IndexKind,
    DiskEngine, DiskStore, PersistenceEngine,
};

fn refused(what: &str, act: impl FnOnce()) {
    assert!(catch_unwind(AssertUnwindSafe(act)).is_err(), "{what}: done instead of panicking");
}

fn height(model: &Model) -> Height {
    model.tip().expect("a block held").height
}

#[test]
fn staged_then_applied_blocks_read_like_their_models_and_every_misuse_panics() {
    let store = DiskEngine::new(SimFs::new()).open(Path::new("/t"), &schema()).expect("open");
    let mut tiered = Tiered::new(store, NonZeroUsize::new(3).expect("non-zero"));
    let empty = Model::default();

    // bulk: final blocks staged in the store's buffer until a batch's weight, then one commit
    let mut staged = empty.clone();
    assert!(!tiered.stage(staged.advance(2, &[1], 1), 1), "1 of 3 bytes");
    let first = staged.clone();
    assert!(tiered.stage(staged.advance(1, &[2], 2), 2), "3 of 3 bytes: a batch");
    staged.assert_view(&tiered.view(), "staged: readable");
    empty.assert_view(tiered.view().durable(), "staged: not durable");
    let tips = (tiered.durable_tip(), tiered.applied(), tiered.staged());
    assert_eq!(tips, (None, None, staged.tip()), "staged: durable, applied, staged");
    let next = staged.clone().advance(0, &[], 0);
    refused("apply over staged", || tiered.apply(next));
    refused("reorg with staged", || tiered.reorg());
    refused("finalize splitting staged", || tiered.finalize(height(&first)));
    tiered.check("staged after misuse");
    tiered.finalize(height(&staged));
    staged.assert_view(tiered.view().durable(), "finalized");
    tiered.check("finalized");

    // tip: applied blocks reorgable, finalized through any one of them
    let durable = staged;
    let mut tip = durable.clone();
    tiered.apply(tip.advance(1, &[3], 1));
    let oldest = tip.clone();
    tiered.apply(tip.advance(0, &[4], 1));
    let mut ahead = tip.clone();
    ahead.advance(0, &[], 0);
    let gap = ahead.advance(0, &[], 0);
    let next = tip.clone().advance(0, &[], 0);
    let other = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest);
    let foreign = Changes::new(next.tip(), &other);
    // probed ids: 0..3 staged, 3 and 4 applied
    let mut twice = next.clone();
    twice.insert(PROBED, &probed_key(4), &[0; 4]);
    refused("a gap", || tiered.apply(gap));
    refused("another schema", || tiered.apply(foreign));
    refused("a key held twice", || tiered.apply(twice));
    refused("final above applied", || _ = tiered.stage(next, 1));
    refused("finalize at durable", || tiered.finalize(height(&durable)));
    refused("finalize above held", || tiered.finalize(height(&tip).next()));
    tip.assert_view(&tiered.view(), "applied after misuse");
    tiered.check("applied after misuse");

    tiered.finalize(height(&oldest));
    oldest.assert_view(tiered.view().durable(), "oldest applied finalized");
    tip.assert_view(&tiered.view(), "newest still applied above it");
    tiered.check("partly finalized");
    tiered.reorg();
    oldest.assert_view(&tiered.view(), "reorg: durable alone");
    let tips = (tiered.durable_tip(), tiered.applied(), tiered.staged());
    assert_eq!(tips, (oldest.tip(), oldest.tip(), None), "reorg: durable, applied, staged");
    let mut replacement = oldest.clone();
    tiered.apply(replacement.advance(2, &[5], 1));
    replacement.assert_view(&tiered.view(), "applies continue after a reorg");
    tiered.check("after reorg");
}

#[test]
fn every_tiered_invariant_broken_by_hand_is_caught_by_name() {
    let tiered = || {
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/t"), &schema()).expect("open");
        let mut tiered = Tiered::new(store, NonZeroUsize::MIN);
        let mut model = Model::default();
        tiered.apply(model.advance(2, &[1, 2], 2));
        tiered.apply(model.advance(1, &[3], 1));
        tiered
    };
    type Drill = (&'static str, fn(&mut Tiered<DiskStore>));
    let drills: [Drill; 3] = [
        ("staged and applied blocks at once", |tiered| tiered.weight = 1),
        ("applied contiguous above durable", |tiered| drop(tiered.applied.pop_front())),
        ("layer = the applied blocks folded", |tiered| tiered.layer = Layer::empty(&schema())),
    ];

    tiered().check("intact");
    for (expected, break_it) in drills {
        let mut tiered = tiered();
        break_it(&mut tiered);
        let caught = catch_unwind(AssertUnwindSafe(|| tiered.check("drill")));
        let message = panic_message(caught.expect_err(expected));
        assert!(message.contains(expected), "{expected}: {message}");
    }
}
