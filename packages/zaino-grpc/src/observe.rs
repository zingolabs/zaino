//! Per-stream measurement, timed over the whole stream.
//!
//! Sits outside admission, so a refusal is counted as the request it was. The close-out runs in
//! the body's `Drop`: a client that walks away mid-stream is measured like any other ending.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use tonic::{Code, Status};

use crate::admission::AtCapacity;
use crate::emit::{self, Method};
use crate::report;

/// gRPC framing: compression flag + big-endian length.
const FRAME_HEADER: usize = 5;

/// Times each request and counts what its body wrote.
#[derive(Clone, Debug)]
pub(crate) struct Measured<Inner> {
    inner: Inner,
}

impl<Inner> Measured<Inner> {
    pub(crate) fn new(inner: Inner) -> Self {
        Self { inner }
    }
}

/// `grpc-status` as a code, when a header map carries one.
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

        // Clone-then-swap is tower's readiness contract: the clone polled ready is called.
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            let response = inner.call(request).await?;
            // Header-carried `grpc-status` = a unary answer or an error, complete already.
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

/// A response body measured to its end, wherever that end comes from.
pub(crate) struct Counted<Inner> {
    inner: Inner,
    method: Method,
    started: Instant,
    /// Admission to the first DATA frame
    first_message: Option<Duration>,
    sent: u64,
    messages: Messages,
    code: Option<Code>,
    /// The status message, when the status is not `Ok`
    message: Option<String>,
    /// Refused by admission (counted, never logged one by one)
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
                // Trailers are where a streaming answer's real status lands.
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
        // No status anywhere and no clean end = the client went away mid-stream.
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

/// Counts whole gRPC messages across data chunks that need not split on a frame boundary.
#[derive(Debug, Default)]
struct Messages {
    complete: u64,
    /// Payload bytes still owed to the message being walked.
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
            let len = u32::from_be_bytes([
                self.header[1],
                self.header[2],
                self.header[3],
                self.header[4],
            ]) as usize;
            match len {
                0 => self.complete += 1,
                len => self.owed = len,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream's message count is the messages, not the chunks it arrived in: one chunk can
    /// carry a run of records, and a record can straddle two.
    #[test]
    fn messages_are_counted_across_chunk_boundaries() {
        let framed = |payload: &[u8]| {
            let mut frame = vec![0u8];
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(payload);
            frame
        };

        // Three records in one chunk, as a range walk hands them over.
        let mut run = Messages::default();
        let packed: Vec<u8> = [framed(b"aa"), framed(b"bbbb"), framed(b"c")].concat();
        run.feed(&packed);
        assert_eq!(run.complete, 3);

        // The same bytes, cut mid-header and mid-payload.
        let mut split = Messages::default();
        let (head, tail) = packed.split_at(3);
        split.feed(head);
        assert_eq!(split.complete, 0, "nothing is complete inside a header");
        let (middle, rest) = tail.split_at(4);
        split.feed(middle);
        split.feed(rest);
        assert_eq!(split.complete, 3, "same run, same count");

        // An empty message is still a message.
        let mut empty = Messages::default();
        empty.feed(&framed(b""));
        assert_eq!(empty.complete, 1);
    }
}
