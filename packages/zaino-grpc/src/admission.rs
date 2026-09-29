//! Stream admission: a permit, or `UNAVAILABLE` with pushback. Never a queue.
//!
//! The permit is owned by the response *body*, not by the handler future, so it is returned
//! when the stream ends **or** when the client disconnects — hyper drops the body either way. A
//! permit released when the handler returned would let unbounded streams accumulate behind a
//! cap that reads as full.
//!
//! - Two pools by [`Class`]: a subscription idles for a block, so it never holds a work permit
//!   (one mempool stream per wallet would otherwise fill `max_streams` with idlers)

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderValue, Request, Response};
use http_body::{Body, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;

use crate::emit;

/// Retry hint on a refusal, in ms.
///
/// Held streams are wallet syncs, so a permit frees in well under a second; a longer hint idles
/// a wallet behind a cap that has already cleared.
const PUSHBACK_MS: &str = "250";

/// gRPC's own "come back later" header (grpc-core: `grpc-retry-pushback-ms`).
const PUSHBACK_HEADER: &str = "grpc-retry-pushback-ms";

/// Which cap a request counts against
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Bounded work: unary, ranges, scans
    Work,
    /// Open until the next block, idle between arrivals (`GetMempoolStream`)
    Subscription,
}

impl Class {
    fn of(path: &str) -> Self {
        match path {
            crate::router::path::GET_MEMPOOL_STREAM => Self::Subscription,
            _ => Self::Work,
        }
    }
}

/// The two admission pools, shared by every connection
#[derive(Clone, Debug)]
pub(crate) struct Permits {
    work: Arc<Semaphore>,
    subscriptions: Arc<Semaphore>,
}

impl Permits {
    pub(crate) fn new(limits: &crate::GrpcLimits) -> Self {
        Self {
            work: Arc::new(Semaphore::new(limits.max_streams.get())),
            subscriptions: Arc::new(Semaphore::new(limits.max_subscriptions.get())),
        }
    }

    fn pool(&self, class: Class) -> &Arc<Semaphore> {
        match class {
            Class::Work => &self.work,
            Class::Subscription => &self.subscriptions,
        }
    }
}

/// Takes a permit of the request's class, or refuses; one per connection (`stalls` = its watch).
#[derive(Clone, Debug)]
pub(crate) struct Admission<Inner> {
    inner: Inner,
    permits: Permits,
    stalls: Arc<crate::stall::Watch>,
}

impl<Inner> Admission<Inner> {
    pub(crate) fn new(inner: Inner, permits: Permits, stalls: Arc<crate::stall::Watch>) -> Self {
        Self { inner, permits, stalls }
    }
}

/// `UNAVAILABLE` plus the pushback hint, written before any handler runs.
fn refused(class: Class) -> Response<tonic::body::Body> {
    let message = match class {
        Class::Work => "stream limit reached",
        Class::Subscription => "subscription limit reached",
    };
    let mut response = Status::unavailable(message).into_http();
    response.headers_mut().insert(PUSHBACK_HEADER, HeaderValue::from_static(PUSHBACK_MS));

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
    type Future = Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let class = Class::of(request.uri().path());
        // `try_acquire_owned`, never `acquire`: excess load is refused immediately.
        let Ok(permit) = Arc::clone(self.permits.pool(class)).try_acquire_owned() else {
            emit::stream_rejected(class);
            return Box::pin(std::future::ready(Ok(
                refused(class).map(|body| Admitted { inner: body, held: None })
            )));
        };
        let held = Held { _permit: StreamPermit::new(permit, class), ticket: self.stalls.ticket() };

        // Clone-then-swap is tower's readiness contract: the clone polled ready is called.
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            Ok(inner.call(request).await?.map(|body| Admitted { inner: body, held: Some(held) }))
        })
    }
}

/// One admission permit, counted while held.
#[derive(Debug)]
struct StreamPermit {
    _permit: OwnedSemaphorePermit,
    class: Class,
}

impl StreamPermit {
    fn new(permit: OwnedSemaphorePermit, class: Class) -> Self {
        emit::stream_admitted(class);
        Self { _permit: permit, class }
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        emit::stream_released(self.class);
    }
}

/// What an admitted stream holds until it ends: its permit and its standing with the stall watch
#[derive(Debug)]
struct Held {
    _permit: StreamPermit,
    ticket: crate::stall::Ticket,
}

/// A response body that owns its stream permit.
///
/// `held` is `None` only on a refusal (which holds nothing) and once the stream has ended.
#[derive(Debug)]
pub(crate) struct Admitted<Inner> {
    inner: Inner,
    held: Option<Held>,
}

impl<Inner: Body + Unpin> Body for Admitted<Inner> {
    type Data = Inner::Data;
    type Error = Inner::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(held) = self.held.as_mut() {
            held.ticket.pulled();
        }
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(None) => self.held = None,
            Poll::Ready(Some(Ok(frame))) if frame.is_data() => {
                if let Some(held) = self.held.as_mut() {
                    held.ticket.handed();
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
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::{Full, StreamBody};

    use super::*;

    /// `/open` never ends; `/data` is one frame then waits; `/short` is one frame and done.
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
            let body = match request.uri().path() {
                "/open" | crate::router::path::GET_MEMPOOL_STREAM => {
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
        Request::builder()
            .uri(format!("http://localhost{path}"))
            .body(Full::new(Bytes::new()))
            .expect("request")
    }

    /// A body that handed hyper a frame and is not polled again (its client stopped reading)
    /// trips the connection's stall watch at the timeout; one waiting on nothing never does, and
    /// a poll clears it
    #[tokio::test(start_paused = true)]
    async fn an_unpulled_frame_trips_the_stall_watch_and_an_idle_stream_never_does() {
        use http_body_util::BodyExt as _;
        use tower::Service as _;

        let timeout = Duration::from_secs(300);
        let stalls = Arc::new(crate::stall::Watch::default());
        let limits = crate::GrpcLimits::default();
        let mut admission = Admission::new(Answers, Permits::new(&limits), Arc::clone(&stalls));
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

    /// A permit lives for as long as its *body*, so it comes back when a client walks away
    /// mid-stream; a request that cannot get one is refused on the spot, with pushback, rather
    /// than queued behind the stream holding it. Subscriptions draw on their own pool: a full
    /// work pool admits one, and a full subscription pool refuses only subscriptions.
    #[tokio::test]
    async fn a_stream_permit_outlives_its_handler_and_a_refusal_never_queues() {
        use http_body_util::BodyExt as _;
        use tower::Service as _;

        let one = std::num::NonZeroUsize::MIN;
        let limits =
            crate::GrpcLimits { max_streams: one, max_subscriptions: one, ..Default::default() };
        let permits = Permits::new(&limits);
        let (streams, subscriptions) = (&permits.work, &permits.subscriptions);
        let mut admission = Admission::new(Answers, permits.clone(), Arc::default());

        // Admitted: the handler has already returned, but the stream is still open.
        let open = admission.call(request("/open")).await.expect("infallible");
        assert_eq!(streams.available_permits(), 0, "the open stream holds it");

        let mempool = crate::router::path::GET_MEMPOOL_STREAM;
        let subscribed = admission.call(request(mempool)).await.expect("infallible");
        let status = subscribed.headers().get("grpc-status");
        assert_eq!(status, None, "a full work pool leaves subscriptions admitted");
        assert_eq!(subscriptions.available_permits(), 0, "the subscription holds its own");
        let second = admission.call(request(mempool)).await.expect("infallible");
        let refusal = Status::from_header_map(second.headers()).expect("a status");
        let refusal = (refusal.code(), refusal.message());
        assert_eq!(refusal, (tonic::Code::Unavailable, "subscription limit reached"));
        drop(subscribed);
        assert_eq!(subscriptions.available_permits(), 1, "returned on disconnect");

        // Refused while it is held — a queue would hang here instead.
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
        assert_eq!(streams.available_permits(), 0, "a refusal takes no permit of its own");

        // The client walks away: dropping the body is the only signal, and it must be enough.
        drop(open);
        assert_eq!(streams.available_permits(), 1, "returned on disconnect");

        // A stream read to its end returns the permit at the end, not at the drop.
        let short = admission.call(request("/short")).await.expect("infallible");
        let mut body = std::pin::pin!(short.into_body());
        assert_eq!(streams.available_permits(), 0);
        while body.frame().await.is_some() {}
        assert_eq!(streams.available_permits(), 1, "returned at end of stream");
    }
}
