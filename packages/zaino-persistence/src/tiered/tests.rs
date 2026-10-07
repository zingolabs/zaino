//! Fire drills: each [`Tiered::check`] invariant broken by hand, caught by name (a check that
//! never fires = a silent pass after every conformance step)

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};

use super::*;
use crate::{
    conformance::{panic_message, schema, Model, PROBED},
    fs::SimFs,
    DiskEngine, DiskStore, PersistenceEngine,
};

#[test]
fn every_held_invariant_broken_by_hand_is_caught_by_name() {
    let tiered = || {
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/t"), &schema()).expect("open");
        let mut tiered = Tiered::new(store, NonZeroUsize::MIN);
        let mut model = Model::default();
        tiered.apply(model.commit(2, &[1, 2], 2));
        tiered.apply(model.commit(1, &[3], 1));
        tiered
    };
    fn probed(tiered: &mut Tiered<DiskStore>) -> &mut OrdMap<Bytes, Bytes> {
        &mut tiered.view.held.maps[usize::from(PROBED.0)]
    }
    type Drill = (&'static str, fn(&mut Tiered<DiskStore>));
    let drills: [Drill; 5] = [
        ("held blocks contiguous above durable", |tiered| {
            tiered.view.held.blocks.pop_front();
        }),
        ("sequence 0 held = its blocks' appends", |tiered| {
            tiered.view.held.sequences[0].push_back(Bytes::from_static(b"stray"));
        }),
        ("map 1 held = its blocks' keys", |tiered| {
            let key = probed(tiered).keys().next().cloned().expect("a probed row held");
            probed(tiered).remove(&key);
        }),
        ("map 1 held = its blocks' keys", |tiered| {
            probed(tiered).insert(Bytes::from(vec![0xee; 16]), Bytes::from_static(b"rows"));
        }),
        ("staged bytes, no block held", |tiered| {
            tiered.staged = Some(1);
            tiered.view.held = Held::empty(&schema());
        }),
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
