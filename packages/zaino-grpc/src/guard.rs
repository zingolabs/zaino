//! Per-stream guard: admission slot, stall ticket, connection activity + close-out metrics, all
//! owned by the response body
//!
//! - body dropped = stream over, however it ended (stream end, client gone: hyper drops it either
//!   way) → slot back, close-out measured
//! - refusal measured as the request it was (counted, never logged one by one)

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame, SizeHint};
use tonic::{Code, Status};
use tower::ServiceExt as _;

use crate::admission::{self, AdmissionState, Class, Slot};
use crate::lifetime::{Activity, Open};
use crate::service::Method;
use crate::stall::{self, Ticket};
use crate::{metrics, summary};

/// One per connection (`stalls`, `activity` = its own)
#[derive(Clone, Debug)]
pub(crate) struct Guard<Inner> {
    inner: Inner,
    admission: Arc<AdmissionState>,
    stalls: Arc<stall::Watch>,
    activity: Arc<Activity>,
}

impl<Inner> Guard<Inner> {
    pub(crate) fn new(
        inner: Inner,
        admission: Arc<AdmissionState>,
        stalls: Arc<stall::Watch>,
        activity: Arc<Activity>,
    ) -> Self {
        Self { inner, admission, stalls, activity }
    }
}

/// `grpc-status`, plus its decoded `grpc-message` when it is not `Ok` (never read on success)
fn status_of(headers: &HeaderMap) -> Option<(Code, Option<String>)> {
    let code = Code::from_i32(headers.get("grpc-status")?.to_str().ok()?.parse().ok()?);
    let message = (code != Code::Ok)
        .then(|| Status::from_header_map(headers).map(|status| status.message().to_owned()))
        .flatten();
    Some((code, message))
}

/// Never pushes back itself: `inner` readied inside the call (`oneshot`)
impl<Inner, ReqBody> tower::Service<Request<ReqBody>> for Guard<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<tonic::body::Body>>
        + Clone
        + Send
        + 'static,
    Inner::Error: Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Guarded<tonic::body::Body>>;
    type Error = Inner::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let method = Method::of_request(&request);
        let class = method.map_or(Class::Work, |method| method.route().class);
        let mut close = CloseOut {
            method,
            started: Instant::now(),
            first_message: None,
            sent: 0,
            code: None,
            message: None,
            at_capacity: false,
            ended: false,
            _open: self.activity.opened(),
        };
        let ticket = self.stalls.ticket();
        let Some(slot) = self.admission.admit(class) else {
            (close.code, close.at_capacity) = (Some(Code::Unavailable), true);
            let refusal = admission::refused(class);
            let refusal = refusal.map(|inner| Guarded { inner, slot: None, ticket, close });
            return Box::pin(std::future::ready(Ok(refusal)));
        };

        let inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.oneshot(request).await?;
            // header-carried `grpc-status` = unary answer or error, already complete
            let (code, message) = status_of(response.headers()).unzip();
            (close.code, close.message) = (code, message.flatten());
            Ok(response.map(|inner| Guarded { inner, slot: Some(slot), ticket, close }))
        })
    }
}

/// Response body owning its stream's guards (`slot` = `None` on a refusal or once ended)
#[derive(Debug)]
pub(crate) struct Guarded<B> {
    inner: B,
    slot: Option<Slot>,
    ticket: Ticket,
    close: CloseOut,
}

/// One stream's measurement to its end, recorded on drop
///
/// - `first_message` = admission → first DATA frame
/// - `message` = non-`Ok` status message
#[derive(Debug)]
struct CloseOut {
    method: Option<Method>,
    started: Instant,
    first_message: Option<Duration>,
    sent: u64,
    code: Option<Code>,
    message: Option<String>,
    at_capacity: bool,
    ended: bool,
    _open: Open,
}

impl<B: Body<Data = Bytes> + Unpin> Body for Guarded<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        this.ticket.pulled();
        if let Some(slot) = this.slot.as_mut() {
            slot.polled();
        }
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        let close = &mut this.close;
        match &polled {
            Poll::Ready(Some(Ok(frame))) => match frame.data_ref() {
                Some(chunk) => {
                    this.ticket.handed();
                    if let Some(slot) = this.slot.as_mut() {
                        slot.handed();
                    }
                    if close.first_message.is_none() {
                        let elapsed = close.started.elapsed();
                        close.first_message = Some(elapsed);
                        metrics::first_message(close.method, elapsed);
                    }
                    close.sent += chunk.len() as u64;
                }
                // trailers = a streaming answer's real status
                None => {
                    if let Some((code, message)) = frame.trailers_ref().and_then(status_of) {
                        (close.code, close.message) = (Some(code), message);
                    }
                }
            },
            Poll::Ready(Some(Err(_)) | None) => {
                this.slot = None;
                close.ended = true;
            }
            Poll::Pending => {}
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

impl Drop for CloseOut {
    fn drop(&mut self) {
        // no status, no clean end = client gone mid-stream
        let code = self.code.unwrap_or(match self.ended {
            true => Code::Ok,
            false => Code::Cancelled,
        });
        let elapsed = self.started.elapsed();
        // answered work request's latency = its client's wait for the first message
        let subscription = self.method.is_some_and(|method| method.route().is_subscription());
        let latency =
            (code == Code::Ok && !subscription).then(|| self.first_message.unwrap_or(elapsed));

        metrics::stream_finished(self.method, code, elapsed, latency, self.sent);
        if code != Code::Ok && !self.at_capacity {
            summary::answered_badly(self.method, code, self.message.as_deref().unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use http::HeaderValue;
    use http_body_util::{BodyExt as _, Full, StreamBody};
    use tower::Service as _;

    use super::*;
    use crate::overload::Signals;
    use crate::AdmissionReport;

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
        let timeout = Duration::from_secs(300);
        let stalls = Arc::new(stall::Watch::default());
        let admission = Arc::new(AdmissionState::new(&crate::GrpcLimits::default()));
        let mut guard = Guard::new(Answers, admission, Arc::clone(&stalls), Arc::default());
        let watching = |stalls: &Arc<stall::Watch>| {
            let stalls = Arc::clone(stalls);
            tokio::spawn(async move { stalls.stalled(timeout).await })
        };

        let idle = guard.call(request("/open")).await.expect("infallible");
        let mut idle = std::pin::pin!(idle.into_body());
        let waiting = tokio::time::timeout(Duration::from_millis(1), idle.frame()).await;
        assert!(waiting.is_err(), "nothing to send");
        let quiet = watching(&stalls);
        tokio::time::sleep(timeout * 3).await;
        assert!(!quiet.is_finished(), "an idle stream owes nothing");
        quiet.abort();

        let data = guard.call(request("/data")).await.expect("infallible");
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
        let one = std::num::NonZeroUsize::MIN;
        let limits = crate::GrpcLimits {
            max_work_streams: one,
            max_subscriptions: one,
            ..Default::default()
        };
        let state = Arc::new(AdmissionState::new(&limits));
        let mut guard = Guard::new(Answers, Arc::clone(&state), Arc::default(), Arc::default());
        let in_flight = || state.report().in_flight;

        // Admitted: handler returned, stream still open
        let open = guard.call(request("/open")).await.expect("infallible");
        assert_eq!(in_flight(), 1, "the open stream holds it");

        let mempool = &Method::GetMempoolStream.path();
        let subscribed = guard.call(request(mempool)).await.expect("infallible");
        let status = subscribed.headers().get("grpc-status");
        assert_eq!(status, None, "a full work pool leaves subscriptions admitted");
        let second = guard.call(request(mempool)).await.expect("infallible");
        let refusal = Status::from_header_map(second.headers()).expect("a status");
        let refusal = (refusal.code(), refusal.message());
        assert_eq!(refusal, (Code::Unavailable, "subscription limit reached"));
        drop(subscribed);
        let third = guard.call(request(mempool)).await.expect("infallible");
        assert_eq!(third.headers().get("grpc-status"), None, "returned on disconnect");

        // Refused while held (a queue would hang here)
        let refused =
            tokio::time::timeout(Duration::from_millis(500), guard.call(request("/open")))
                .await
                .expect("a refusal is immediate, never queued behind the open stream")
                .expect("infallible");
        let headers = refused.headers();
        let pushback = (headers.get("grpc-status"), headers.get("grpc-retry-pushback-ms"));
        let (unavailable, retry_after) = (HeaderValue::from_static("14"), "250".parse().ok());
        assert_eq!(pushback, (Some(&unavailable), retry_after.as_ref()), "refused + retry");
        assert_eq!(in_flight(), 1, "a refusal takes no slot of its own");

        // Client walks away: body drop = the only signal
        drop(open);
        assert_eq!(in_flight(), 0, "returned on disconnect");

        // Read to its end: slot back at the end, not at the drop
        let short = guard.call(request("/short")).await.expect("infallible");
        let mut body = std::pin::pin!(short.into_body());
        assert_eq!(in_flight(), 1);
        while body.frame().await.is_some() {}
        assert_eq!(in_flight(), 0, "returned at end of stream");
    }

    /// Default limits (`max_work_streams` 2048, adaptive from 256):
    ///
    /// - 257th open work stream refused below the ceiling, `UNAVAILABLE` + pushback
    /// - report says so once the window closes
    /// - subscription never held to the work limit
    #[tokio::test]
    async fn work_past_the_adaptive_limit_is_refused_below_the_ceiling() {
        let state = Arc::new(AdmissionState::new(&crate::GrpcLimits::default()));
        let mut guard = Guard::new(Answers, Arc::clone(&state), Arc::default(), Arc::default());
        let mut open = Vec::new();
        for _ in 0..256 {
            open.push(guard.call(request("/open")).await.expect("infallible"));
        }
        let refused = guard.call(request("/open")).await.expect("infallible");
        let status = Status::from_header_map(refused.headers()).expect("a status");
        let pushback = refused.headers().get("grpc-retry-pushback-ms");
        let refusal = (status.code(), status.message(), pushback);
        let retry_after = HeaderValue::from_static("250");
        assert_eq!(refusal, (Code::Unavailable, "work stream limit reached", Some(&retry_after)));

        let refusals = state.tick(Duration::from_secs(1), Signals::default());
        assert_eq!(refusals, 1, "the window's one refusal");
        let report = state.report();
        let expected =
            AdmissionReport { limit: 256, in_flight: 256, refused_per_s: 1.0, overload: None };
        assert_eq!(report, expected, "refused below the 2048 ceiling");

        let mempool = Method::GetMempoolStream.path();
        let subscribed = guard.call(request(&mempool)).await.expect("infallible");
        assert_eq!(subscribed.headers().get("grpc-status"), None, "subscriptions: own pool");
    }
}
