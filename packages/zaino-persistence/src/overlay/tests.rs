//! Fire drills: each [`Overlay::check`] invariant broken by hand, caught by name (check never
//! firing = silent pass after every conformance step)

use std::panic::{catch_unwind, AssertUnwindSafe};

use super::*;
use crate::conformance::{panic_message, Model, PROBED, SCHEMA};

#[test]
fn every_layer_invariant_broken_by_hand_is_caught_by_name() {
    let layer = || {
        let mut model = Model::default();
        let layer = Overlay::empty(&SCHEMA).with(&model.advance(2, &[1, 2], 2, &[]));
        layer.with(&model.advance(1, &[3], 1, &[]))
    };
    fn probed(layer: &mut Overlay) -> &mut OrdMap<Bytes, (Height, Entry)> {
        &mut layer.maps[usize::from(PROBED.id.0)]
    }
    type Drill = (&'static str, fn(&mut Overlay));
    let drills: [Drill; 5] = [
        ("layer blocks ascending", |layer| {
            let oldest = layer.deltas.pop_front().expect("two blocks");
            layer.deltas.push_back(oldest);
        }),
        ("sequence 0 = its blocks' appends", |layer| {
            layer.sequences[0].push_back(Bytes::from_static(b"stray"));
        }),
        ("map 1 = its blocks' keys", |layer| {
            let key = probed(layer).keys().next().cloned().expect("a probed row held");
            probed(layer).remove(&key);
        }),
        ("map 1 = its blocks' keys", |layer| {
            let tip = layer.deltas.back().expect("two blocks").tip.height;
            let stray = (tip, Entry::Value(Bytes::from_static(b"rows")));
            probed(layer).insert(Bytes::from(vec![0xee; 16]), stray);
        }),
        // rebase dropping the newer block would then keep its key: a removed value resurrected
        ("map 1 each entry owned by the newest block writing it", |layer| {
            let newest = layer.deltas.back().expect("two blocks").tip.height;
            let owned = probed(layer).iter().find(|(_, (owner, _))| *owner == newest);
            let key = owned.expect("newest block wrote an id").0.clone();
            let oldest = layer.deltas.front().expect("two blocks").tip.height;
            probed(layer).get_mut(&key).expect("held").0 = oldest;
        }),
    ];

    layer().check("intact");
    for (expected, break_it) in drills {
        let mut layer = layer();
        break_it(&mut layer);
        let caught = catch_unwind(AssertUnwindSafe(|| layer.check("drill")));
        let message = panic_message(caught.expect_err(expected));
        assert!(message.contains(expected), "{expected}: {message}");
    }
}
