//! Fire drills (`verified-chain.md` §10 layer 4): each check in `Holders::check()` and each
//! precondition, seen firing on a planted bug

use std::num::NonZeroU32;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use zaino_header_chain::testing::HeaderViews;
use zaino_primitives::testing::{h as height, MockChain};
use zaino_primitives::types::{Height, ReorgDepth};

use super::Holders;
use zaino_traffic::ValidatorId;

use crate::endpoints::Agreement;

/// Panic message of `run`, `None` = it returned
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = catch_unwind(AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}

/// Verified 0..=10 (depth 3), a fork at 9′: validator 0 agreed (answered 7 and 10), 1 behind at 5,
/// 2 polled on the fork then lost; every check passes, then one planted bug per check
#[test]
fn every_holders_check_fires_on_its_planted_bug() {
    let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nz"));
    let mut world = MockChain::regtest();
    let tip = world.mine_empty(10);
    let trunk: Vec<_> = (0..=10).map(|at| world.at(height(at))).collect();
    let at = |h: u32| trunk[h as usize];
    let fork = world.fork(height(8)).mine_empty(1).tip();
    let verified = Arc::new(world.verified(tip));
    let endpoint = |at: usize| ValidatorId::new(at).expect("< MAX");
    let valid = || {
        let mut holders = Holders::new(3, depth);
        holders.verified(Some(Arc::clone(&verified)));
        holders.polled(endpoint(0), at(10), vec![at(7), at(10)]);
        holders.polled(endpoint(1), at(5), Vec::new());
        holders.polled(endpoint(2), fork, vec![at(7)]);
        holders.lost(endpoint(2));
        holders
    };
    assert_eq!(fired(|| valid().check()), None, "the unplanted standing passes");
    let agreements = [0, 1, 2].map(|at| valid().agreement(endpoint(at)));
    assert_eq!(agreements, [Agreement::Agreed, Agreement::Behind, Agreement::Unknown]);

    type Plant = Box<dyn Fn(&mut Holders)>;
    let drills: Vec<(&str, Plant)> = vec![
        (
            "holders: validator 0's poll answers ascending",
            Box::new(|h| {
                let answers = h.validators[0].answers.as_mut().expect("polled");
                answers.polled.reverse();
            }),
        ),
        ("V1: validator 1's reach", Box::new(|h| h.validators[1].reach = Some(height(10)))),
        ("V1: validator 2's reach", Box::new(|h| h.validators[2].reach = Some(Height::GENESIS))),
        (
            "V2: validator 0's agreement",
            Box::new(|h| h.validators[0].agreement = Agreement::Diverged),
        ),
    ];
    for (expected, plant) in drills {
        let mut holders = valid();
        plant(&mut holders);
        let message = fired(|| holders.check()).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }

    // preconditions: a caller bug panics naming the invariant
    let unverified = Holders::new(3, depth);
    let (b6, b7, b10) = (at(6), at(7), at(10));
    let preconditions: [(&str, Plant); 5] = [
        (
            "holders: 3 not a configured validator",
            Box::new(move |h| h.polled(endpoint(3), b10, Vec::new())),
        ),
        (
            "holders: poll answers ascending, at most ASKED",
            Box::new(move |h| h.polled(endpoint(0), b10, vec![b10, b7])),
        ),
        (
            "holders: poll answers ascending, at most ASKED",
            Box::new(move |h| h.polled(endpoint(0), b10, vec![b6, b7, b10])),
        ),
        (
            "V1: holders asked of a verified block",
            Box::new(move |h| {
                h.holders(fork);
            }),
        ),
        (
            "V1: holders asked under a verified chain",
            Box::new(move |h| {
                *h = unverified.clone();
                h.holders(b10);
            }),
        ),
    ];
    for (expected, call) in preconditions {
        let mut holders = valid();
        let message = fired(|| call(&mut holders)).unwrap_or_default();
        assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
    }
    let held = valid().holders(at(7));
    assert_eq!(held.positions().collect::<Vec<_>>(), [0], "a valid call passes: 1 holds ≤ 5");
}
