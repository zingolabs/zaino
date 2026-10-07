//! Per-stream measurement, timed over the whole stream
//!
//! - Outside admission (a refusal counted as the request it was)
//! - Close-out in the body's `Drop` (a client walking away mid-stream = measured like any end)

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use tonic::{Code, Status};
use zaino_proto::frame::{framed_len, FRAME_HEADER};

use crate::admission::AtCapacity;
use crate::emit::{self, Method};
use crate::report;

/// Each request timed, its body's writes counted
#[derive(Clone, Debug)]
pub(crate) struct Measured<Inner> {
    inner: Inner,
}

impl<Inner> Measured<Inner> {
    pub(crate) fn new(inner: Inner) -> Self {
        Self { inner }
    }
}

/// `grpc-status` as a code, if present
fn code_of(headers: &http::HeaderMap) -> Option<Code> {
    let status = headers.get("grpc-status")?.to_str().ok()?;
    Some(Code::from_i32(status.parse().ok()?))
}

/// `grpc-status`, plus its decoded `grpc-message` when it is not `Ok` (never read on success)
fn status_of(headers: &http::HeaderMap) -> Option<(Code, Option<String>)> {
    let code = code_of(headers)?;
    let message = (code != Code::Ok)
        .then(|| Status::from_header_map(headers).map(|status| status.message().to_owned()))
        .flatten();
    Some((code, message))
}

impl<Inner, ReqBody, ResBody> tower::Service<Request<ReqBody>> for Measured<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Counted<ResBody>>;
    type Error = Inner::Error;
    type Future = Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let method = Method::of(request.uri().path());
        let started = Instant::now();

        // Clone-then-swap = tower's readiness contract (the instance polled ready is called)
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            let response = inner.call(request).await?;
            // Header-carried `grpc-status` = unary answer or error, already complete
            let (code, message) = status_of(response.headers()).unzip();
            let at_capacity = response.extensions().get::<AtCapacity>().is_some();

            Ok(response.map(|body| Counted {
                inner: body,
                method,
                started,
                first_message: None,
                sent: 0,
                messages: Messages::default(),
                code,
                message: message.flatten(),
                at_capacity,
                ended: false,
            }))
        })
    }
}

/// Response body measured to its end, wherever that comes from
///
/// - `first_message` = admission → first DATA frame; `message` = non-`Ok` status message
/// - `at_capacity` = admission refusal (counted, never logged one by one)
pub(crate) struct Counted<Inner> {
    inner: Inner,
    method: Method,
    started: Instant,
    first_message: Option<Duration>,
    sent: u64,
    messages: Messages,
    code: Option<Code>,
    message: Option<String>,
    at_capacity: bool,
    ended: bool,
}

impl<Inner: Body<Data = bytes::Bytes> + Unpin> Body for Counted<Inner> {
    type Data = Inner::Data;
    type Error = Inner::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);

        match &polled {
            Poll::Ready(Some(Ok(frame))) => match frame.data_ref() {
                Some(chunk) => {
                    if self.first_message.is_none() {
                        let elapsed = self.started.elapsed();
                        self.first_message = Some(elapsed);
                        emit::first_message(self.method, elapsed);
                    }
                    self.sent += chunk.len() as u64;
                    self.messages.feed(chunk);
                }
                // Trailers = a streaming answer's real status
                None => {
                    if let Some((code, message)) = frame.trailers_ref().and_then(status_of) {
                        (self.code, self.message) = (Some(code), message);
                    }
                }
            },
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => self.ended = true,
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

impl<Inner> Drop for Counted<Inner> {
    fn drop(&mut self) {
        // No status, no clean end = client gone mid-stream
        let code = self.code.unwrap_or(match self.ended {
            true => Code::Ok,
            false => Code::Cancelled,
        });

        let elapsed = self.started.elapsed();
        // an answered work request's latency = what its client waited for the first message
        let latency = (code == Code::Ok && !self.method.is_subscription())
            .then(|| self.first_message.unwrap_or(elapsed));

        emit::stream_finished(
            self.method,
            code,
            elapsed,
            latency,
            self.messages.complete,
            self.sent,
        );
        if code != Code::Ok && !self.at_capacity {
            report::answered_badly(self.method, code, self.message.as_deref().unwrap_or_default());
        }
    }
}

/// Whole gRPC messages across chunks not split on frame boundaries (`owed` = payload bytes left
/// in the message being walked)
#[derive(Debug, Default)]
struct Messages {
    complete: u64,
    owed: usize,
    header: [u8; FRAME_HEADER],
    seen: usize,
}

impl Messages {
    fn feed(&mut self, chunk: &[u8]) {
        let mut at = 0;
        while at < chunk.len() {
            if self.owed > 0 {
                let step = self.owed.min(chunk.len() - at);
                self.owed -= step;
                at += step;
                if self.owed == 0 {
                    self.complete += 1;
                }
                continue;
            }

            self.header[self.seen] = chunk[at];
            self.seen += 1;
            at += 1;
            if self.seen < FRAME_HEADER {
                continue;
            }

            self.seen = 0;
            match framed_len(&self.header).map(|framed| framed - FRAME_HEADER) {
                Some(0) => self.complete += 1,
                Some(len) => self.owed = len,
                // header whole: `None` only on a 32-bit `usize` overflow (no real message)
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Messages, not chunks (one chunk = a run of records; a record may straddle two)
    #[test]
    fn messages_are_counted_across_chunk_boundaries() {
        let framed = |payload: &[u8]| {
            let mut frame = Vec::new();
            zaino_proto::frame::frame_into(&mut frame, |out| out.extend_from_slice(payload));
            frame
        };

        // Three records in one chunk (as a range walk hands them over)
        let mut run = Messages::default();
        let packed: Vec<u8> = [framed(b"aa"), framed(b"bbbb"), framed(b"c")].concat();
        run.feed(&packed);
        assert_eq!(run.complete, 3);

        // Same bytes, cut mid-header + mid-payload
        let mut split = Messages::default();
        let (head, tail) = packed.split_at(3);
        split.feed(head);
        assert_eq!(split.complete, 0, "nothing is complete inside a header");
        let (middle, rest) = tail.split_at(4);
        split.feed(middle);
        split.feed(rest);
        assert_eq!(split.complete, 3, "same run, same count");

        // Empty message = still a message
        let mut empty = Messages::default();
        empty.feed(&framed(b""));
        assert_eq!(empty.complete, 1);
    }
}
