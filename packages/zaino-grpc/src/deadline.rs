//! Time bounds on one request: the client's `grpc-timeout`, and an idle bound, never a total cap
//!
//! - deadline = `grpc-timeout` at arrival: past it, a pending handler = `DEADLINE_EXCEEDED`, a
//!   stream ends in `DEADLINE_EXCEEDED` trailers
//! - idle = nothing to send for `stall_timeout` while hyper waits on the handler or the body →
//!   `UNAVAILABLE` (retry); the client-side twin = `stall.rs` (sent, unread → connection closed)
//! - `GetMempoolStream` never idles out (silent until the next block by design)
//! - a long stream that keeps delivering lives as long as the client wants (Z#1537)

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Response};
use http_body::Frame;
use tokio::time::{Instant, Sleep};
use tonic::{body::Body, Status};

use crate::wire::{status_response, trailers};

/// `grpc-timeout` (gRPC over HTTP/2: <= 8 digits + `H M S m u n`); malformed = none, as tonic reads
/// it
fn requested(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("grpc-timeout")?.to_str().ok()?;
    let (digits, unit) = value.split_at_checked(value.len().checked_sub(1)?)?;
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    match unit {
        "H" => Some(Duration::from_secs(amount * 3_600)),
        "M" => Some(Duration::from_secs(amount * 60)),
        "S" => Some(Duration::from_secs(amount)),
        "m" => Some(Duration::from_millis(amount)),
        "u" => Some(Duration::from_micros(amount)),
        "n" => Some(Duration::from_nanos(amount)),
        _ => None,
    }
}

/// One request's bounds, fixed at arrival (`idle` = `None`: never idles out)
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    deadline: Option<Instant>,
    idle: Option<Duration>,
}

impl Bounds {
    pub(crate) fn of(headers: &HeaderMap, idle: Option<Duration>) -> Self {
        Self { deadline: requested(headers).map(|timeout| Instant::now() + timeout), idle }
    }

    /// `answer` cut at the first bound; a stream's body (status in trailers) bounded the same
    pub(crate) async fn apply(
        self,
        answer: impl Future<Output = Response<Body>>,
    ) -> Response<Body> {
        let first_message = self.idle.map(|idle| Instant::now() + idle);
        let cut = match (self.deadline, first_message) {
            (Some(deadline), Some(idle)) => Some(deadline.min(idle)),
            (deadline, idle) => deadline.or(idle),
        };
        let response = match cut {
            Some(at) => match tokio::time::timeout_at(at, answer).await {
                Ok(response) => response,
                Err(_) => return status_response(self.cut_at(at)),
            },
            None => answer.await,
        };
        // grpc-status in the headers = complete (unary or a refusal): nothing left to bound
        if response.headers().contains_key("grpc-status") {
            return response;
        }
        let deadline = self.deadline.map(|at| Box::pin(tokio::time::sleep_until(at)));
        let (limit, idle) = (self.idle, None);
        response.map(|inner| Body::new(Bounded { inner: Some(inner), deadline, limit, idle }))
    }

    /// Why a cut at `at` ended the request
    fn cut_at(&self, at: Instant) -> Status {
        match (self.deadline == Some(at), self.idle) {
            (false, Some(idle)) => idled(idle),
            _ => expired(),
        }
    }
}

fn expired() -> Status {
    Status::deadline_exceeded("grpc-timeout passed")
}

fn idled(idle: Duration) -> Status {
    Status::unavailable(format!("nothing to send for {idle:?}"))
}

/// A stream body under its [`Bounds`] (`inner` dropped at a cut: its reads and permits go with it)
///
/// - `idle` runs from the first `Pending` since the last frame, never while hyper holds one
struct Bounded {
    inner: Option<Body>,
    deadline: Option<Pin<Box<Sleep>>>,
    limit: Option<Duration>,
    idle: Option<Pin<Box<Sleep>>>,
}

impl Bounded {
    fn cut(&mut self, status: Status) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        self.inner = None;
        Poll::Ready(Some(Ok(Frame::trailers(trailers(&status)))))
    }
}

impl http_body::Body for Bounded {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else { return Poll::Ready(None) };
        if this.deadline.as_ref().is_some_and(|deadline| deadline.is_elapsed()) {
            return this.cut(expired());
        }
        match Pin::new(inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.idle = None;
                if frame.is_trailers() {
                    this.inner = None;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(other) => {
                this.inner = None;
                Poll::Ready(other)
            }
            Poll::Pending => {
                if let Some(deadline) = this.deadline.as_mut() {
                    if deadline.as_mut().poll(cx).is_ready() {
                        return this.cut(expired());
                    }
                }
                if let Some(limit) = this.limit {
                    let idle = this.idle.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                    if idle.as_mut().poll(cx).is_ready() {
                        return this.cut(idled(limit));
                    }
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, Response};
    use http_body::Frame;
    use http_body_util::{BodyExt as _, StreamBody};
    use tokio::time::Instant;
    use tonic::{body::Body, Status};

    use super::{requested, Bounds};
    use crate::wire::trailers;

    fn timeout(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-timeout", HeaderValue::from_static(value));
        headers
    }

    /// gRPC over HTTP/2's `TimeoutValue TimeoutUnit`; anything else = no deadline (tonic's)
    #[test]
    fn grpc_timeout_reads_up_to_eight_digits_and_a_unit() {
        let secs = Duration::from_secs;
        let cases = [
            ("1H", Some(secs(3_600))),
            ("2M", Some(secs(120))),
            ("30S", Some(secs(30))),
            ("100m", Some(Duration::from_millis(100))),
            ("30000000u", Some(secs(30))),
            ("7n", Some(Duration::from_nanos(7))),
            ("99999999S", Some(secs(99_999_999))),
            ("123456789S", None),
            ("S", None),
            ("5", None),
            ("5x", None),
            ("+5S", None),
            ("-5S", None),
            (" 5S", None),
        ];
        for (value, expected) in cases {
            assert_eq!(requested(&timeout(value)), expected, "{value:?}");
        }
        assert_eq!(requested(&HeaderMap::new()), None, "absent");
    }

    /// Every frame of `response` with the second it arrived (paused clock from `start`)
    async fn drained(start: Instant, response: Response<Body>) -> Vec<(u64, String)> {
        if let Some(status) = Status::from_header_map(response.headers()) {
            return vec![(start.elapsed().as_secs(), format!("{:?}", status.code()))];
        }
        let mut body = std::pin::pin!(response.into_body());
        let mut frames = Vec::new();
        while let Some(frame) = body.frame().await {
            let at = start.elapsed().as_secs();
            match frame.expect("in-band").into_trailers() {
                Ok(trailers) => {
                    let status = Status::from_header_map(&trailers).expect("a status");
                    frames.push((at, format!("{:?}", status.code())));
                }
                Err(_) => frames.push((at, "data".to_owned())),
            }
        }
        frames
    }

    /// A stream: one record every 100 s, `count` of them, then `OK` trailers; or silent after them
    fn every_100_s(count: u64, then_silent: bool) -> Response<Body> {
        use std::cmp::Ordering::{Equal, Greater, Less};

        let frames = futures::stream::unfold(0, move |sent: u64| async move {
            let frame = match sent.cmp(&count) {
                Less => {
                    tokio::time::sleep(Duration::from_secs(100)).await;
                    Frame::data(Bytes::from_static(b"\0\0\0\0\0"))
                }
                Equal if then_silent => return std::future::pending().await,
                Equal => Frame::trailers(trailers(&Status::ok(""))),
                Greater => return None,
            };
            Some((Ok::<_, Status>(frame), sent + 1))
        });
        Response::new(Body::new(StreamBody::new(Box::pin(frames))))
    }

    /// Z#1537, R9 (paused clock, idle = 300 s):
    /// - no total cap: 1,000 s of a record every 100 s ends `OK`
    /// - `grpc-timeout` 250 s cuts the same stream after two records, `DEADLINE_EXCEEDED`
    /// - nothing to send for 300 s = `UNAVAILABLE`; no idle bound (a subscription) = never cut
    /// - a handler pending past its deadline / the idle bound = that status, in the headers
    #[tokio::test(start_paused = true)]
    async fn a_stream_lives_while_it_delivers_until_the_clients_deadline_or_an_idle_bound() {
        let idle = Some(Duration::from_secs(300));
        let data = |at: u64| (at, "data".to_owned());
        let ended = |at: u64, code: &str| (at, code.to_owned());

        let start = Instant::now();
        let bounds = Bounds::of(&HeaderMap::new(), idle);
        let long = drained(start, bounds.apply(async { every_100_s(10, false) }).await).await;
        let all: Vec<_> = (1..=10).map(|n| data(n * 100)).chain([ended(1_000, "Ok")]).collect();
        assert_eq!(long, all, "1,000 s, never cut");

        let start = Instant::now();
        let bounds = Bounds::of(&timeout("250S"), idle);
        let cut = drained(start, bounds.apply(async { every_100_s(10, false) }).await).await;
        assert_eq!(cut, [data(100), data(200), ended(250, "DeadlineExceeded")]);

        let start = Instant::now();
        let bounds = Bounds::of(&HeaderMap::new(), idle);
        let quiet = drained(start, bounds.apply(async { every_100_s(1, true) }).await).await;
        assert_eq!(quiet, [data(100), ended(400, "Unavailable")], "300 s after the last record");

        let start = Instant::now();
        let bounds = Bounds::of(&HeaderMap::new(), None);
        let subscribed = bounds.apply(async { every_100_s(1, true) });
        let subscribed = drained(start, subscribed.await);
        let open = tokio::time::timeout(Duration::from_secs(3_000), subscribed).await;
        assert!(open.is_err(), "no idle bound: open for as long as it takes");

        for (headers, expected) in [
            (timeout("2S"), ended(2, "DeadlineExceeded")),
            (HeaderMap::new(), ended(300, "Unavailable")),
            (timeout("1H"), ended(300, "Unavailable")),
        ] {
            let start = Instant::now();
            let pending = Bounds::of(&headers, idle).apply(std::future::pending());
            assert_eq!(drained(start, pending.await).await, [expected]);
        }
    }

    /// The router reads each request's own `grpc-timeout`: a `GetTaddressBalanceStream` whose
    /// client never ends its request stream = `DEADLINE_EXCEEDED` at the header's 500 ms (else
    /// at the 30 s request-body deadline)
    #[tokio::test(start_paused = true)]
    async fn the_router_cuts_each_request_at_its_own_grpc_timeout() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_persistence::IndexKind;
        use zaino_proto::proto::service as proto;

        use crate::service::Routes;
        use crate::testing::{dispatch, indexed, routes, snapshot};
        use crate::wire::path;

        let chain = zaino_primitives::testing::Chain::new();
        let blocks = chain.path(chain.genesis().hash);
        let views = vec![indexed(IndexKind::TransparentAddress, &blocks)];
        let mut router = dispatch(Routes { snapshots: snapshot(&blocks, views), ..routes() });

        let alice = proto::Address { address: "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned() };
        let mut framed = Vec::new();
        zaino_proto::frame::frame_into(&mut framed, |out| alice.encode_raw(out));
        for (header, expected) in [
            (Some("500m"), (500, "grpc-timeout passed".to_owned())),
            (None, (30_000, "request body not complete within 30s".to_owned())),
        ] {
            let first = Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from(framed.clone())));
            let unending = futures::StreamExt::chain(
                futures::stream::iter([first]),
                futures::stream::pending(),
            );
            let mut request = http::Request::builder()
                .uri(format!("http://localhost{}", path::GET_TADDRESS_BALANCE_STREAM));
            if let Some(header) = header {
                request = request.header("grpc-timeout", header);
            }
            let request = request.body(StreamBody::new(unending)).expect("request");
            let start = Instant::now();
            let response = router.call(request).await.expect("router answers");
            let status = Status::from_header_map(response.headers()).expect("a status");
            let cut = (start.elapsed().as_millis(), status.message().to_owned());
            assert_eq!(status.code(), tonic::Code::DeadlineExceeded, "{header:?}");
            assert_eq!(cut, expected, "{header:?}");
        }
    }
}
