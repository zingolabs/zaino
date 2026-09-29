//! What an [`IndexFollower`](crate::IndexFollower) reads: one [`Subscription`], or a [`Zip`] of
//! two subscriptions where the second is published step for step from the first
//!
//! - Zip = lockstep: one step off each stream per step, asserted to be the same step

use std::future::Future;
use std::sync::Arc;

use zaino_primitives::types::{BlockHash, Height};

use crate::{Linked, Step, Subscription, Weight};

mod sealed {
    pub trait Sealed {}
}

/// A follower's input stream (sealed: [`Subscription`] and [`Zip`] only)
pub trait Feed: sealed::Sealed + Send + 'static {
    type Item;

    /// Next step; cancel-safe (the follower races it against a landing write)
    fn next(&mut self) -> impl Future<Output = Step<Self::Item>> + Send;

    /// Next step if one is already queued (never waits)
    fn try_next(&mut self) -> Option<Step<Self::Item>>;

    /// Nothing queued (the follower's idle point)
    fn is_idle(&self) -> bool;

    /// Every queue popped through its `Shutdown` (none dropped before; a no-op once there)
    fn skip_to_shutdown(&mut self) -> impl Future<Output = ()> + Send;
}

impl<T: Send + Sync + 'static> sealed::Sealed for Subscription<T> {}

impl<T: Send + Sync + 'static> Feed for Subscription<T> {
    type Item = T;

    async fn next(&mut self) -> Step<T> {
        Subscription::next(self).await
    }

    fn try_next(&mut self) -> Option<Step<T>> {
        Subscription::try_next(self)
    }

    fn is_idle(&self) -> bool {
        self.is_empty()
    }

    async fn skip_to_shutdown(&mut self) {
        Subscription::skip_to_shutdown(self).await;
    }
}

/// A derived item names the upstream item it was derived from
pub trait DerivedFrom<A> {
    fn derived_from(&self, upstream: &A) -> bool;
}

/// One upstream item and the item derived from it: what a [`Zip`] yields per `Apply`
#[derive(Debug)]
pub struct Paired<A, B> {
    pub upstream: Arc<A>,
    pub derived: Arc<B>,
}

impl<A: Linked, B> Linked for Paired<A, B> {
    fn height(&self) -> Height {
        self.upstream.height()
    }

    fn hash(&self) -> BlockHash {
        self.upstream.hash()
    }

    fn prev_hash(&self) -> BlockHash {
        self.upstream.prev_hash()
    }
}

impl<A: Weight, B: Weight> Weight for Paired<A, B> {
    fn weight(&self) -> usize {
        self.upstream.weight() + self.derived.weight()
    }
}

/// An upstream subscription read in lockstep with a derived one published from it
///
/// - Derived `Shutdown` before upstream's = its publisher failed → `Shutdown` (the failure is
///   the publisher's to report; the upstream queue is popped through its own `Shutdown` by
///   [`skip_to_shutdown`](Feed::skip_to_shutdown))
/// - Any other mismatch = a bug in the publisher → panic
pub struct Zip<A, B> {
    upstream: Subscription<A>,
    derived: Subscription<B>,
    /// Upstream step popped before its derived twin was queued (kept across a cancelled wait)
    held: Option<Step<A>>,
}

impl<A, B> Zip<A, B> {
    pub fn new(upstream: Subscription<A>, derived: Subscription<B>) -> Self {
        Self { upstream, derived, held: None }
    }
}

impl<A, B: DerivedFrom<A>> Zip<A, B> {
    fn paired(upstream: Step<A>, derived: Step<B>) -> Step<Paired<A, B>> {
        match (upstream, derived) {
            (
                Step::Apply { height, finalized, data },
                Step::Apply { height: derived_height, finalized: derived_finalized, data: item },
            ) => {
                assert_eq!(
                    (derived_height, derived_finalized),
                    (height, finalized),
                    "derived Apply out of step with upstream"
                );
                assert!(item.derived_from(&data), "derived item at {height} from another block");
                Step::Apply {
                    height,
                    finalized,
                    data: Arc::new(Paired { upstream: data, derived: item }),
                }
            }
            (Step::Finalized { height }, Step::Finalized { height: derived_height }) => {
                assert_eq!(derived_height, height, "derived Finalized out of step with upstream");
                Step::Finalized { height }
            }
            (Step::Reset, Step::Reset) => Step::Reset,
            (_, Step::Shutdown) => Step::Shutdown,
            (upstream, derived) => panic!(
                "derived stream out of step: upstream {}, derived {}",
                kind(&upstream),
                kind(&derived)
            ),
        }
    }
}

fn kind<T>(step: &Step<T>) -> &'static str {
    match step {
        Step::Apply { .. } => "Apply",
        Step::Finalized { .. } => "Finalized",
        Step::Reset => "Reset",
        Step::Shutdown => "Shutdown",
    }
}

impl<A: Send + Sync + 'static, B: Send + Sync + 'static> sealed::Sealed for Zip<A, B> {}

impl<A, B> Feed for Zip<A, B>
where
    A: Send + Sync + 'static,
    B: DerivedFrom<A> + Send + Sync + 'static,
{
    type Item = Paired<A, B>;

    async fn next(&mut self) -> Step<Paired<A, B>> {
        if self.held.is_none() {
            self.held = Some(self.upstream.next().await);
        }
        let derived = self.derived.next().await;
        let upstream = self.held.take().expect("held across the derived wait");
        Self::paired(upstream, derived)
    }

    fn try_next(&mut self) -> Option<Step<Paired<A, B>>> {
        let upstream = match self.held.take() {
            Some(step) => step,
            None => self.upstream.try_next()?,
        };
        match self.derived.try_next() {
            Some(derived) => Some(Self::paired(upstream, derived)),
            None => {
                self.held = Some(upstream);
                None
            }
        }
    }

    fn is_idle(&self) -> bool {
        self.held.is_none() && self.upstream.is_empty()
    }

    async fn skip_to_shutdown(&mut self) {
        self.held = None;
        // concurrently: a publisher blocked on a full derived queue never reaches its Shutdown
        tokio::join!(self.upstream.skip_to_shutdown(), self.derived.skip_to_shutdown());
    }
}
