//! Stream admission: a slot, or `UNAVAILABLE` + pushback, never a queue
//!
//! - Slot owned by the response *body*, not the handler (back on stream end or disconnect: hyper
//!   drops the body either way)
//! - released at handler return = unbounded streams
//! - Two pools by [`Class`]: a subscription idles for a block, so it never holds a work slot (one
//!   mempool stream per wallet would otherwise fill `max_work_streams` with idlers)
//! - Work held exactly to the adaptive limit (`adaptive.rs`), fed each message's production time

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use http::{HeaderValue, Request, Response};
use http_body::{Body, Frame, SizeHint};
use tokio::time::Instant;
use tonic::Status;

use crate::adaptive::Limiter;
use crate::layer::take_ready;
use crate::metrics;
use crate::overload::Overload;
use crate::service::Method;

/// Refusal retry hint, ms (held streams = wallet syncs: a slot frees well under a second)
const PUSHBACK_MS: &str = "250";

/// gRPC's own "come back later" header (grpc-core)
const PUSHBACK_HEADER: &str = "grpc-retry-pushback-ms";

/// Cap a request counts against (each method's in [`ROUTES`](crate::service::ROUTES), unknown
/// path = `Work`)
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
    max_work_streams: usize,
    max_subscriptions: usize,
    limiter: Limiter,
    overload: Overload,
}

/// Work pool
///
/// - `limit` = the adaptive one (`max_work_streams` when off)
/// - `refused_per_s`, `overloaded` as of the last serving summary (thresholds: `overload.rs`)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdmissionReport {
    pub limit: usize,
    pub in_flight: usize,
    pub refused_per_s: f64,
    pub overloaded: bool,
}

impl AdmissionState {
    pub(crate) fn new(limits: &crate::GrpcLimits) -> Self {
        let max_work_streams = limits.max_work_streams.get();
        Self {
            work: AtomicUsize::new(0),
            subscriptions: AtomicUsize::new(0),
            max_work_streams,
            max_subscriptions: limits.max_subscriptions.get(),
            limiter: Limiter::new(max_work_streams, limits.adaptive_work_streams),
            overload: Overload::default(),
        }
    }

    pub fn report(&self) -> AdmissionReport {
        AdmissionReport {
            limit: self.limiter.limit(),
            in_flight: self.held(Class::Work).0,
            refused_per_s: self.overload.refused_per_s(),
            overloaded: self.overload.overloaded(),
        }
    }

    /// Closes a serving summary's window (`elapsed` long): its refusals, every class
    pub(crate) fn tick(&self, elapsed: Duration) -> u64 {
        metrics::adaptive_stream_limit(self.limiter.limit());
        self.overload.tick(elapsed)
    }

    fn pool(&self, class: Class) -> (&AtomicUsize, usize) {
        match class {
            Class::Work => (&self.work, self.max_work_streams),
            Class::Subscription => (&self.subscriptions, self.max_subscriptions),
        }
    }

    /// Streams of `class` held now, of the pool's size
    pub(crate) fn held(&self, class: Class) -> (usize, usize) {
        let (held, max) = self.pool(class);
        (held.load(Ordering::Relaxed), max)
    }

    /// A slot, or refused at once at the class's limit (work: the adaptive one)
    fn admit(&self, class: Class) -> bool {
        let (held, max) = self.pool(class);
        let limit = match class {
            Class::Work => self.limiter.limit(),
            Class::Subscription => max,
        };
        let taken = |held: usize| (held < limit).then_some(held + 1);
        let admitted = held.fetch_update(Ordering::Relaxed, Ordering::Relaxed, taken).is_ok();
        match admitted {
            true => {
                self.overload.admitted(class);
                metrics::stream_admitted(class);
            }
            false => self.overload.refused(class),
        }
        admitted
    }

    fn release(&self, class: Class) {
        self.pool(class).0.fetch_sub(1, Ordering::Relaxed);
        metrics::stream_released(class);
    }
}

/// Slot of the request's class, or refusal
///
/// - one per connection (`stalls` = its watch)
#[derive(Clone, Debug)]
pub(crate) struct Admission<Inner> {
    inner: Inner,
    admission: Arc<AdmissionState>,
    stalls: Arc<crate::stall::Watch>,
}

impl<Inner> Admission<Inner> {
    pub(crate) fn new(
        inner: Inner,
        admission: Arc<AdmissionState>,
        stalls: Arc<crate::stall::Watch>,
    ) -> Self {
        Self { inner, admission, stalls }
    }
}

/// Marks a response as an admission refusal (counted as such, never logged one by one)
#[derive(Clone, Copy, Debug)]
pub(crate) struct AtCapacity;

/// `UNAVAILABLE` + pushback hint, before any handler runs
fn refused(class: Class) -> Response<tonic::body::Body> {
    let message = match class {
        Class::Work => "work stream limit reached",
        Class::Subscription => "subscription limit reached",
    };
    let mut response = Status::unavailable(message).into_http();
    response.headers_mut().insert(PUSHBACK_HEADER, HeaderValue::from_static(PUSHBACK_MS));
    response.extensions_mut().insert(AtCapacity);

    response
}

impl<Inner, ReqBody> tower::Service<Request<ReqBody>> for Admission<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<tonic::body::Body>>
        + Clone
        + Send
        + 'static,
    Inner::Error: Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Admitted<tonic::body::Body>>;
    type Error = Inner::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let class = Method::of_request(&request).map_or(Class::Work, |method| method.route().class);
        if !self.admission.admit(class) {
            let refusal = refused(class).map(|body| Admitted { inner: body, holding: None });
            return Box::pin(std::future::ready(Ok(refusal)));
        }
        let holding = StreamHolding {
            class,
            ticket: self.stalls.ticket(),
            admission: Arc::clone(&self.admission),
            busy_since: Some(Instant::now()),
        };

        let mut inner = take_ready(&mut self.inner);
        Box::pin(async move {
            let response = inner.call(request).await?;
            Ok(response.map(|body| Admitted { inner: body, holding: Some(holding) }))
        })
    }
}

/// What an admitted stream holds until it ends: its slot, its standing with the stall watch, and
/// since when it has been producing its next message (`None` = handed one, not polled since)
#[derive(Debug)]
struct StreamHolding {
    class: Class,
    ticket: crate::stall::Ticket,
    admission: Arc<AdmissionState>,
    busy_since: Option<Instant>,
}

impl StreamHolding {
    fn polled(&mut self) {
        self.ticket.pulled();
        self.busy_since.get_or_insert_with(Instant::now);
    }

    /// A data frame out: one work message's production time to the limiter
    fn handed(&mut self) {
        self.ticket.handed();
        let since = self.busy_since.take();
        if let (Class::Work, Some(since)) = (self.class, since) {
            let in_flight = self.admission.held(Class::Work).0;
            self.admission.limiter.sample(since.elapsed(), in_flight);
        }
    }
}

impl Drop for StreamHolding {
    fn drop(&mut self) {
        self.admission.release(self.class);
    }
}

/// Response body owning its slot (`holding` = `None` only on a refusal or once ended)
#[derive(Debug)]
pub(crate) struct Admitted<Inner> {
    inner: Inner,
    holding: Option<StreamHolding>,
}

impl<Inner: Body + Unpin> Body for Admitted<Inner> {
    type Data = Inner::Data;
    type Error = Inner::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(holding) = self.holding.as_mut() {
            holding.polled();
        }
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(None) => self.holding = None,
            Poll::Ready(Some(Ok(frame))) if frame.is_data() => {
                if let Some(holding) = self.holding.as_mut() {
                    holding.handed();
                }
            }
            _ => {}
        }

        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use bytes::Bytes;
    use http_body_util::{Full, StreamBody};

    use super::*;

    /// - `/open` + `GetMempoolStream` never end
    /// - `/data` = one frame then waits
    /// - `/short` = one frame, done
    #[derive(Clone)]
    struct Answers;

    impl tower::Service<Request<Full<Bytes>>> for Answers {
        type Response = Response<tonic::body::Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request<Full<Bytes>>) -> Self::Future {
            let path = request.uri().path();
            let body = match path {
                _ if path == "/open" || path == Method::GetMempoolStream.path() => {
                    tonic::body::Body::new(StreamBody::new(futures::stream::pending::<
                        Result<Frame<Bytes>, Status>,
                    >()))
                }
                "/data" => {
                    let first = Ok::<_, Status>(Frame::data(Bytes::from_static(b"\0\0\0\0\0")));
                    let then = futures::stream::pending();
                    tonic::body::Body::new(StreamBody::new(futures::StreamExt::chain(
                        futures::stream::iter([first]),
                        then,
                    )))
                }
                _ => tonic::body::Body::new(Full::new(Bytes::from_static(b"\0\0\0\0\0"))),
            };

            std::future::ready(Ok(Response::new(body)))
        }
    }

    fn request(path: &str) -> Request<Full<Bytes>> {
        let mut request = Request::builder()
            .uri(format!("http://localhost{path}"))
            .body(Full::new(Bytes::new()))
            .expect("request");
        Method::resolve(&mut request);
        request
    }

    /// - Frame handed over, never polled again (client stopped reading) → stall at the timeout
    /// - Waiting on nothing never trips
    /// - Poll clears it
    #[tokio::test(start_paused = true)]
    async fn an_unpulled_frame_trips_the_stall_watch_and_an_idle_stream_never_does() {
        use http_body_util::BodyExt as _;
        use tower::Service as _;

        let timeout = Duration::from_secs(300);
        let stalls = Arc::new(crate::stall::Watch::default());
        let limits = crate::GrpcLimits::default();
        let admission = Arc::new(AdmissionState::new(&limits));
        let mut admission = Admission::new(Answers, admission, Arc::clone(&stalls));
        let watching = |stalls: &Arc<crate::stall::Watch>| {
            let stalls = Arc::clone(stalls);
            tokio::spawn(async move { stalls.stalled(timeout).await })
        };

        let idle = admission.call(request("/open")).await.expect("infallible");
        let mut idle = std::pin::pin!(idle.into_body());
        let waiting = tokio::time::timeout(Duration::from_millis(1), idle.frame()).await;
        assert!(waiting.is_err(), "nothing to send");
        let quiet = watching(&stalls);
        tokio::time::sleep(timeout * 3).await;
        assert!(!quiet.is_finished(), "an idle stream owes nothing");
        quiet.abort();

        let data = admission.call(request("/data")).await.expect("infallible");
        let mut data = std::pin::pin!(data.into_body());
        assert!(data.frame().await.expect("a frame").expect("ok").is_data());
        let tripped = watching(&stalls);
        tokio::time::sleep(timeout).await;
        tripped.await.expect("unpulled for the timeout: stalled");

        let pulled = tokio::time::timeout(Duration::from_millis(1), data.frame()).await;
        assert!(pulled.is_err(), "polled again (and now waiting): no longer owing");
        let cleared = watching(&stalls);
        tokio::time::sleep(timeout * 3).await;
        assert!(!cleared.is_finished(), "a pull clears it");
    }

    /// - Slot lives as long as its *body* (back when a client walks away mid-stream)
    /// - No slot → refused on the spot + pushback, never queued
    /// - Subscriptions = own pool (full work pool admits one, full subscription pool refuses only
    ///   subscriptions)
    #[tokio::test]
    async fn a_stream_slot_outlives_its_handler_and_a_refusal_never_queues() {
        use http_body_util::BodyExt as _;
        use tower::Service as _;

        let one = std::num::NonZeroUsize::MIN;
        let limits = crate::GrpcLimits {
            max_work_streams: one,
            max_subscriptions: one,
            ..Default::default()
        };
        let state = Arc::new(AdmissionState::new(&limits));
        let mut admission = Admission::new(Answers, Arc::clone(&state), Arc::default());
        let held = |class| state.held(class).0;

        // Admitted: handler returned, stream still open
        let open = admission.call(request("/open")).await.expect("infallible");
        assert_eq!(held(Class::Work), 1, "the open stream holds it");

        let mempool = &Method::GetMempoolStream.path();
        let subscribed = admission.call(request(mempool)).await.expect("infallible");
        let status = subscribed.headers().get("grpc-status");
        assert_eq!(status, None, "a full work pool leaves subscriptions admitted");
        assert_eq!(held(Class::Subscription), 1, "the subscription holds its own");
        let second = admission.call(request(mempool)).await.expect("infallible");
        let refusal = Status::from_header_map(second.headers()).expect("a status");
        let refusal = (refusal.code(), refusal.message());
        assert_eq!(refusal, (tonic::Code::Unavailable, "subscription limit reached"));
        drop(subscribed);
        assert_eq!(held(Class::Subscription), 0, "returned on disconnect");

        // Refused while held (a queue would hang here)
        let refused =
            tokio::time::timeout(Duration::from_millis(500), admission.call(request("/open")))
                .await
                .expect("a refusal is immediate, never queued behind the open stream")
                .expect("infallible");
        let headers = refused.headers();
        let (status, pushback) = (headers.get("grpc-status"), headers.get(PUSHBACK_HEADER));
        let unavailable = HeaderValue::from_static("14");
        let retry_after = HeaderValue::from_static(PUSHBACK_MS);
        assert_eq!((status, pushback), (Some(&unavailable), Some(&retry_after)), "refused + retry");
        assert_eq!(held(Class::Work), 1, "a refusal takes no slot of its own");

        // Client walks away: body drop = the only signal
        drop(open);
        assert_eq!(held(Class::Work), 0, "returned on disconnect");

        // Read to its end: slot back at the end, not at the drop
        let short = admission.call(request("/short")).await.expect("infallible");
        let mut body = std::pin::pin!(short.into_body());
        assert_eq!(held(Class::Work), 1);
        while body.frame().await.is_some() {}
        assert_eq!(held(Class::Work), 0, "returned at end of stream");
    }

    /// Default limits (`max_work_streams` 2048, adaptive from 256):
    ///
    /// - 257th open work stream refused below the ceiling, `UNAVAILABLE` + pushback
    /// - report says so once the window closes
    /// - subscription never held to the work limit
    #[tokio::test]
    async fn work_past_the_adaptive_limit_is_refused_below_the_ceiling() {
        use tower::Service as _;

        let state = Arc::new(AdmissionState::new(&crate::GrpcLimits::default()));
        let mut admission = Admission::new(Answers, Arc::clone(&state), Arc::default());
        let mut open = Vec::new();
        for _ in 0..256 {
            open.push(admission.call(request("/open")).await.expect("infallible"));
        }
        let refused = admission.call(request("/open")).await.expect("infallible");
        let status = Status::from_header_map(refused.headers()).expect("a status");
        let pushback = refused.headers().get(PUSHBACK_HEADER);
        let refusal = (status.code(), status.message(), pushback);
        let retry_after = HeaderValue::from_static(PUSHBACK_MS);
        let expected = (tonic::Code::Unavailable, "work stream limit reached", Some(&retry_after));
        assert_eq!(refusal, expected);
        assert_eq!(state.held(Class::Work), (256, 2048), "refused below the ceiling");

        assert_eq!(state.tick(Duration::from_secs(1)), 1, "the window's one refusal");
        let report = state.report();
        let expected =
            AdmissionReport { limit: 256, in_flight: 256, refused_per_s: 1.0, overloaded: false };
        assert_eq!(report, expected);

        let mempool = Method::GetMempoolStream.path();
        let subscribed = admission.call(request(&mempool)).await.expect("infallible");
        assert_eq!(subscribed.headers().get("grpc-status"), None, "subscriptions: own pool");
    }
}
