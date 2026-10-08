//! Trace tests: the seam's invariants hold across sequences of publishes, so
//! each test drives a sequence and asserts the chain after every step.

use zaino_primitives::types::{BlockHash, Height};

use crate::{Seam, SeamFault};

/// A distinct hash per height, so a mismatch is detectable in tests.
fn hash(n: u8) -> BlockHash {
    BlockHash::from([n; 32])
}

fn height(n: u32) -> Height {
    Height::try_from(n).expect("test heights are in range")
}

#[test]
fn an_empty_seam_has_neither_quantity() {
    let (horizon, watermark) = Seam::new(100, 10).split();
    assert_eq!(horizon.durable(), None);
    assert_eq!(horizon.retention_floor(), None);
    assert_eq!(watermark.released(), None);
}

#[test]
fn the_horizon_is_the_tip_less_the_reorg_depth() {
    let (mut horizon, watermark) = Seam::new(100, 10).split();
    let released = horizon
        .advance(height(1000), hash(1))
        .expect("first publish is legal");
    assert_eq!(released.height(), height(900));
    assert_eq!(released.hash(), hash(1));
    assert_eq!(watermark.released().map(|r| r.height()), Some(height(900)));
}

#[test]
fn a_tip_below_the_reorg_depth_saturates_at_genesis() {
    let (mut horizon, _watermark) = Seam::new(100, 10).split();
    let released = horizon
        .advance(height(30), hash(1))
        .expect("saturating publish is legal");
    assert_eq!(released.height(), Height::GENESIS);
}

#[test]
fn an_unpublished_horizon_leaves_the_durable_half_with_nothing() {
    // Review Focus 1: no `Released` exists yet, so there is nothing to present.
    // The advance is prevented by the type system, not a runtime check: no
    // `Released` can be constructed outside `ReorgHorizon::advance`, so without a
    // horizon publish there is no value to pass to `DurableWatermark::advance`.
    let (_horizon, watermark) = Seam::new(100, 10).split();
    assert_eq!(watermark.released(), None);
}

#[test]
fn the_watermark_cannot_pass_the_horizon() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    assert_eq!(
        watermark.advance(&released, height(901)),
        Err(SeamFault::WatermarkPastHorizon {
            to: height(901),
            horizon: height(900)
        })
    );
    // Review Focus 3: the rejection left the state untouched.
    assert_eq!(horizon.durable(), None);
}

#[test]
fn the_watermark_cannot_regress() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    watermark.advance(&released, height(800)).expect("legal");
    assert_eq!(
        watermark.advance(&released, height(799)),
        Err(SeamFault::RegressedWatermark {
            to: height(799),
            held: height(800)
        })
    );
    assert_eq!(horizon.durable().map(|c| c.height()), Some(height(800)));
}

#[test]
fn the_horizon_cannot_regress() {
    let (mut horizon, _watermark) = Seam::new(100, 10).split();
    horizon.advance(height(1000), hash(1)).expect("legal");
    assert_eq!(
        horizon.advance(height(999), hash(2)),
        Err(SeamFault::RegressedHorizon {
            to: height(899),
            held: height(900)
        })
    );
}

#[test]
fn republishing_the_same_height_is_legal_and_idempotent() {
    // Review Focus 4: monotonicity is non-decreasing, so equality is not a fault.
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    horizon
        .advance(height(1000), hash(1))
        .expect("equal republish is legal");
    watermark.advance(&released, height(800)).expect("legal");
    watermark
        .advance(&released, height(800))
        .expect("equal republish is legal");
    assert_eq!(horizon.durable().map(|c| c.height()), Some(height(800)));
}

#[test]
fn the_retention_floor_trails_the_watermark_by_the_margin() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    watermark.advance(&released, height(800)).expect("legal");
    assert_eq!(horizon.retention_floor(), Some(height(790)));
}

#[test]
fn a_watermark_below_the_margin_saturates_the_floor_at_genesis() {
    // Review Focus 2: no underflow when the margin exceeds the watermark.
    let (mut horizon, mut watermark) = Seam::new(0, 10).split();
    let released = horizon.advance(height(3), hash(1)).expect("legal");
    watermark.advance(&released, height(3)).expect("legal");
    assert_eq!(horizon.retention_floor(), Some(Height::GENESIS));
}

#[test]
fn the_inequality_chain_holds_across_a_ratchet_run() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    for tip in [1000_u32, 1100, 1200, 1300] {
        let released = horizon.advance(height(tip), hash(1)).expect("legal");
        watermark
            .advance(&released, released.height())
            .expect("following the horizon is legal");

        let floor = horizon.retention_floor().expect("a watermark exists");
        let w = horizon.durable().expect("a watermark exists").height();
        let r = released.height();
        assert!(u32::from(floor) + 10 <= u32::from(w), "floor + margin <= w");
        assert!(u32::from(w) <= u32::from(r), "w <= r");
        assert!(u32::from(r) <= tip - 100, "r <= t - d");
    }
}
