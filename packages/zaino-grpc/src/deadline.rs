//! Time bounds on one request: the client's `grpc-timeout`, and a silence bound, never a total cap
//!
//! - deadline = `grpc-timeout` at arrival: past it, a pending handler = `DEADLINE_EXCEEDED`, a
//!   stream ends in `DEADLINE_EXCEEDED` trailers
//! - silence = nothing to send for `silent_timeout` while hyper waits on the handler or the body
//!   → `UNAVAILABLE` (retry)
//! - client-side twin = `stall.rs` (`unread_timeout` → connection closed)
//! - `GetMempoolStream` exempt (silent until the next block by design)
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

use crate::wire::trailers;

/// `grpc-timeout` (gRPC over HTTP/2: <= 8 digits + `H M S m u n`)
///
/// - malformed = none (as tonic reads it)
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

/// One request's bounds, fixed at arrival (`silent_limit` = `None`: never cut for silence)
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    deadline: Option<Instant>,
    silent_limit: Option<Duration>,
}

impl Bounds {
    pub(crate) fn of(headers: &HeaderMap, silent_limit: Option<Duration>) -> Self {
        Self { deadline: requested(headers).map(|timeout| Instant::now() + timeout), silent_limit }
    }

    /// `answer` cut at the first bound
    ///
    /// - stream's body (status in trailers) bounded the same
    pub(crate) async fn apply(
        self,
        answer: impl Future<Output = Response<Body>>,
    ) -> Response<Body> {
        let first_message = self.silent_limit.map(|limit| Instant::now() + limit);
        let cut = match (self.deadline, first_message) {
            (Some(deadline), Some(silent)) => Some(deadline.min(silent)),
            (deadline, silent) => deadline.or(silent),
        };
        let response = match cut {
            Some(at) => match tokio::time::timeout_at(at, answer).await {
                Ok(response) => response,
                Err(_) => return self.cut_at(at).into_http(),
            },
            None => answer.await,
        };
        // grpc-status in the headers = complete (unary or a refusal): nothing left to bound
        if response.headers().contains_key("grpc-status") {
            return response;
        }
        let deadline = self.deadline.map(|at| Box::pin(tokio::time::sleep_until(at)));
        let (silent_limit, silent_timer) = (self.silent_limit, None);
        response.map(|inner| {
            Body::new(Bounded { inner: Some(inner), deadline, silent_limit, silent_timer })
        })
    }

    /// Why a cut at `at` ended the request
    fn cut_at(&self, at: Instant) -> Status {
        match (self.deadline == Some(at), self.silent_limit) {
            (false, Some(limit)) => silenced(limit),
            _ => expired(),
        }
    }
}

fn expired() -> Status {
    Status::deadline_exceeded("grpc-timeout passed")
}

fn silenced(silent: Duration) -> Status {
    Status::unavailable(format!("nothing to send for {silent:?}"))
}

/// A stream body under its [`Bounds`] (`inner` dropped at a cut: its reads and permits go with it)
///
/// - `silent_timer` runs from the first `Pending` since the last frame, never while hyper holds one
struct Bounded {
    inner: Option<Body>,
    deadline: Option<Pin<Box<Sleep>>>,
    silent_limit: Option<Duration>,
    silent_timer: Option<Pin<Box<Sleep>>>,
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
                this.silent_timer = None;
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
                if let Some(limit) = this.silent_limit {
                    let timer = this
                        .silent_timer
                        .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                    if timer.as_mut().poll(cx).is_ready() {
                        return this.cut(silenced(limit));
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

    /// gRPC over HTTP/2's `TimeoutValue TimeoutUnit`, anything else = no deadline (tonic's)
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

    /// What the handler under test answers
    #[derive(Clone, Copy, Debug)]
    enum Handler {
        /// Never returns a response
        Pending,
        /// `count` records, one every `period`, then `OK` trailers (`silent` = nothing after them)
        Stream { count: u64, period: Duration, silent: bool },
    }

    async fn handle(handler: Handler) -> Response<Body> {
        use std::cmp::Ordering::{Equal, Greater, Less};

        let Handler::Stream { count, period, silent } = handler else {
            return std::future::pending().await;
        };
        let frames = futures::stream::unfold(0, move |sent: u64| async move {
            let frame = match sent.cmp(&count) {
                Less => {
                    tokio::time::sleep(period).await;
                    Frame::data(Bytes::from_static(b"\0\0\0\0\0"))
                }
                Equal if silent => return std::future::pending().await,
                Equal => Frame::trailers(trailers(&Status::ok(""))),
                Greater => return None,
            };
            Some((Ok::<_, Status>(frame), sent + 1))
        });
        Response::new(Body::new(StreamBody::new(Box::pin(frames))))
    }

    /// Z#1537 (paused clock): no total cap, only the client's deadline or the silence bound
    #[tokio::test(start_paused = true)]
    async fn a_stream_lives_while_it_delivers_until_the_clients_deadline_or_a_silence_bound() {
        const SILENT: Option<Duration> = Some(Duration::from_secs(300));
        let s100 = Duration::from_secs(100);
        let data = |at: u64| (at, "data".to_owned());
        let ended = |at: u64, code: &str| (at, code.to_owned());
        let every_100s_for_1000s: Vec<_> = (1..=10).map(|n| data(n * 100)).collect();
        let delivering = Handler::Stream { count: 10, period: s100, silent: false };

        // case, grpc-timeout, silence bound, handler, frames as (second, data / status)
        #[rustfmt::skip]
        let cases = [
            ("delivering 1,000 s",       HeaderMap::new(), SILENT, delivering,
                [every_100s_for_1000s, vec![ended(1_000, "Ok")]].concat()),
            ("client deadline 250 s",    timeout("250S"),  SILENT, delivering,
                vec![data(100), data(200), ended(250, "DeadlineExceeded")]),
            ("silent after one record",  HeaderMap::new(), SILENT, Handler::Stream { count: 1, period: s100, silent: true },
                vec![data(100), ended(400, "Unavailable")]),
            ("subscription, 1,000 s gaps", HeaderMap::new(), None, Handler::Stream { count: 2, period: s100 * 10, silent: false },
                vec![data(1_000), data(2_000), ended(2_000, "Ok")]),
            ("handler pending, 2 s deadline", timeout("2S"), SILENT, Handler::Pending,
                vec![ended(2, "DeadlineExceeded")]),
            ("handler pending, no deadline", HeaderMap::new(), SILENT, Handler::Pending,
                vec![ended(300, "Unavailable")]),
            ("handler pending, 1 h deadline", timeout("1H"), SILENT, Handler::Pending,
                vec![ended(300, "Unavailable")]),
        ];
        for (case, headers, silent, handler, expected) in cases {
            let start = Instant::now();

            let response = Bounds::of(&headers, silent).apply(handle(handler)).await;

            assert_eq!(drained(start, response).await, expected, "{case}: {handler:?}");
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
        use zaino_proto::proto::service;

        use crate::service::Method;
        use crate::testing::{dispatch, fixed_snapshots, indexed, routes_serving, MAINNET};

        let chain = zaino_source::testing::MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, genesis)];
        let held_by = zaino_chainview::TrustedValidatorSet::default();
        let snapshots = fixed_snapshots(&chain, genesis, views, held_by, &[]);
        let indexes = [IndexKind::TransparentAddress];
        let mut router = dispatch(routes_serving(MAINNET, &indexes, snapshots));

        let alice = service::Address { address: "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned() };
        let mut framed = Vec::new();
        zaino_proto::frame::frame_into(&mut framed, |out| alice.encode_raw(out));
        // grpc-timeout, cut after (ms), its message
        for (header, cut_ms, message) in [
            (Some("500m"), 500, "grpc-timeout passed"),
            (None, 30_000, "request body not complete within 30s"),
        ] {
            let first = Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from(framed.clone())));
            let unending = futures::StreamExt::chain(
                futures::stream::iter([first]),
                futures::stream::pending(),
            );
            let mut request = http::Request::builder()
                .uri(format!("http://localhost{}", Method::GetTaddressBalanceStream.path()));
            if let Some(header) = header {
                request = request.header("grpc-timeout", header);
            }
            let mut request = request.body(StreamBody::new(unending)).expect("request");
            Method::resolve(&mut request);
            let start = Instant::now();
            let response = router.call(request).await.expect("router answers");
            let status = Status::from_header_map(response.headers()).expect("Trailers-Only status");
            let cut = (status.code(), status.message(), start.elapsed().as_millis());
            let expected = (tonic::Code::DeadlineExceeded, message, cut_ms);
            assert_eq!(cut, expected, "grpc-timeout {header:?}: (code, message, cut after ms)");
        }
    }
}
