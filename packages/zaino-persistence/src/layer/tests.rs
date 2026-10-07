//! Fire drills: each [`Layer::check`] invariant broken by hand, caught by name (check never
//! firing = silent pass after every conformance step)

use std::panic::{catch_unwind, AssertUnwindSafe};

use super::*;
use crate::conformance::{panic_message, schema, Model, PROBED};

#[test]
fn every_layer_invariant_broken_by_hand_is_caught_by_name() {
    let layer = || {
        let mut model = Model::default();
        let layer = Layer::empty(&schema()).with(&model.advance(2, &[1, 2], 2));
        layer.with(&model.advance(1, &[3], 1))
    };
    fn probed(layer: &mut Layer) -> &mut OrdMap<Bytes, Bytes> {
        &mut layer.maps[usize::from(PROBED.0)]
    }
    type Drill = (&'static str, fn(&mut Layer));
    let drills: [Drill; 4] = [
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
            probed(layer).insert(Bytes::from(vec![0xee; 16]), Bytes::from_static(b"rows"));
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
