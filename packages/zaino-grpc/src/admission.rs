//! Stream admission: a [`Slot`], or `UNAVAILABLE` + pushback, never a queue
//!
//! - Slot owned by the response *body* (`guard.rs`), not the handler (released at handler return
//!   = unbounded streams)
//! - Two pools by [`Class`]: a subscription idles for a block, so it never holds a work slot (one
//!   mempool stream per wallet would otherwise fill `max_work_streams` with idlers)
//! - Work held exactly to the adaptive limit (`adaptive.rs`), fed each message's production time

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderValue, Response};
use tokio::time::Instant;
use tonic::Status;

use crate::adaptive::Limiter;
use crate::metrics;
use crate::overload::{Overload, OverloadCause, Signals};
use crate::OverloadLimits;

/// Refusal retry hint, ms (held streams = wallet syncs: a slot frees well under a second)
const PUSHBACK_MS: &str = "250";

/// gRPC's own "come back later" header (grpc-core)
const PUSHBACK_HEADER: &str = "grpc-retry-pushback-ms";

/// Cap a request counts against (each method's in [`Method::route`](crate::service::Method::route),
/// unknown path = `Work`)
///
/// - `Work` = unary, ranges, scans
/// - `Subscription` = open until the next block, idle between arrivals
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Work,
    Subscription,
}

/// The two admission pools, the adaptive work limit and their outcomes, shared by every connection
/// ([`GrpcService::admission`](crate::GrpcService::admission) = its readiness view)
#[derive(Debug)]
pub struct AdmissionState {
    work: AtomicUsize,
    subscriptions: AtomicUsize,
    max_subscriptions: usize,
    limiter: Limiter,
    overload: Overload,
}

/// Work pool
///
/// - `limit` = the adaptive one (`max_work_streams` when off)
/// - `refused_per_s`, `overload` as of the last serving summary (rules: `overload.rs`)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdmissionReport {
    pub limit: usize,
    pub in_flight: usize,
    pub refused_per_s: f64,
    pub overload: Option<OverloadCause>,
}

impl AdmissionState {
    pub(crate) fn new(limits: &crate::GrpcLimits) -> Self {
        Self {
            work: AtomicUsize::new(0),
            subscriptions: AtomicUsize::new(0),
            max_subscriptions: limits.max_subscriptions.get(),
            limiter: Limiter::new(limits.max_work_streams.get(), limits.adaptive_work_streams),
            overload: Overload::new(limits.overload.clone()),
        }
    }

    pub fn report(&self) -> AdmissionReport {
        AdmissionReport {
            limit: self.limiter.limit(),
            in_flight: self.held(Class::Work),
            refused_per_s: self.overload.refused_per_s(),
            overload: self.overload.cause(),
        }
    }

    /// The operator's overload thresholds + whether a latched cause is reported (next window on)
    pub fn set_overload(&self, limits: OverloadLimits, detection: bool) {
        self.overload.set(limits, detection);
    }

    pub(crate) fn overload_limits(&self) -> OverloadLimits {
        self.overload.limits()
    }

    /// Closes a serving summary's window (`elapsed` long, on its `signals`): its refusals, every
    /// class
    pub(crate) fn tick(&self, elapsed: Duration, signals: Signals) -> u64 {
        metrics::adaptive_stream_limit(self.limiter.limit());
        self.overload.tick(elapsed, signals)
    }

    /// `class`'s pool + its limit now (work: the adaptive one)
    fn pool(&self, class: Class) -> (&AtomicUsize, usize) {
        match class {
            Class::Work => (&self.work, self.limiter.limit()),
            Class::Subscription => (&self.subscriptions, self.max_subscriptions),
        }
    }

    /// Streams of `class` held now
    fn held(&self, class: Class) -> usize {
        self.pool(class).0.load(Ordering::Relaxed)
    }

    /// A slot, or `None` = refused at once at the class's limit
    pub(crate) fn admit(self: &Arc<Self>, class: Class) -> Option<Slot> {
        let (held, limit) = self.pool(class);
        let taken = |held: usize| (held < limit).then_some(held + 1);
        if held.fetch_update(Ordering::Relaxed, Ordering::Relaxed, taken).is_err() {
            self.overload.refused(class);
            return None;
        }
        self.overload.admitted(class);
        metrics::stream_admitted(class);
        Some(Slot { class, admission: Arc::clone(self), busy_since: Some(Instant::now()) })
    }
}

/// One admitted stream's place in its pool, back on drop
///
/// - `busy_since` = producing its next message since (`None` = handed one, not polled since)
#[derive(Debug)]
pub(crate) struct Slot {
    class: Class,
    admission: Arc<AdmissionState>,
    busy_since: Option<Instant>,
}

impl Slot {
    pub(crate) fn polled(&mut self) {
        self.busy_since.get_or_insert_with(Instant::now);
    }

    /// A data frame out: one work message's production time to the limiter
    pub(crate) fn handed(&mut self) {
        let since = self.busy_since.take();
        if let (Class::Work, Some(since)) = (self.class, since) {
            let in_flight = self.admission.held(Class::Work);
            self.admission.limiter.sample(since.elapsed(), in_flight);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.admission.pool(self.class).0.fetch_sub(1, Ordering::Relaxed);
        metrics::stream_released(self.class);
    }
}

/// `UNAVAILABLE` + pushback hint, before any handler runs
pub(crate) fn refused(class: Class) -> Response<tonic::body::Body> {
    let message = match class {
        Class::Work => "work stream limit reached",
        Class::Subscription => "subscription limit reached",
    };
    let mut response = Status::unavailable(message).into_http();
    response.headers_mut().insert(PUSHBACK_HEADER, HeaderValue::from_static(PUSHBACK_MS));
    response
}
