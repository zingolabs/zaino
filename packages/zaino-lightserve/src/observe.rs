//! Per-call measurement, timed over the whole response.
//!
//! [`MeasuredLayer`] wraps the gRPC service. The clock starts when a request
//! reaches it, before the handler runs; the response body is wrapped in
//! [`Counted`], which records the first response message as it is polled out
//! and closes the call out when it drops. Closing out in `Drop` measures a
//! client that walks away mid-stream like any other end.
//!
//! ```text
//! first_message = arrival → first DATA frame
//! duration      = arrival → body dropped (trailers sent, error, or client gone)
//! status        = grpc-status from the trailers (streaming) or headers (unary);
//!                 none and no clean end = Cancelled
//! ```

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame, SizeHint};
use tonic::Code;

use crate::emit::{self, Method};

/// Wraps a service in [`Measured`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MeasuredLayer;

impl<Inner> tower::Layer<Inner> for MeasuredLayer {
    type Service = Measured<Inner>;

    fn layer(&self, inner: Inner) -> Self::Service {
        Measured { inner }
    }
}

/// Each call timed from arrival, its response body counted until it drops.
#[derive(Clone, Debug)]
pub(crate) struct Measured<Inner> {
    inner: Inner,
}

/// `grpc-status` as a code, if present.
fn code_of(headers: &HeaderMap) -> Option<Code> {
    let status = headers.get("grpc-status")?.to_str().ok()?;
    Some(Code::from_i32(status.parse().ok()?))
}

impl<Inner, ReqBody, ResBody> tower::Service<Request<ReqBody>> for Measured<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Counted<ResBody>>;
    type Error = Inner::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let method = Method::of_path(request.uri().path());
        let started = Instant::now();
        // The clone takes the readiness `poll_ready` reserved; the clone left
        // behind is polled ready again before the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let response = inner.call(request).await?;
            // A header-carried status is a unary answer or an error, already
            // complete; a streaming answer's status arrives in the trailers.
            let code = code_of(response.headers());
            Ok(response.map(|body| Counted {
                inner: body,
                method,
                started,
                answered: false,
                sent: 0,
                messages: Messages::default(),
                code,
                ended: false,
            }))
        })
    }
}

/// A response body measured to its end, wherever that comes from.
pub(crate) struct Counted<Inner> {
    inner: Inner,
    method: Method,
    started: Instant,
    /// Whether the first DATA frame has gone out.
    answered: bool,
    sent: u64,
    messages: Messages,
    code: Option<Code>,
    ended: bool,
}

impl<Inner: Body<Data = Bytes> + Unpin> Body for Counted<Inner> {
    type Data = Bytes;
    type Error = Inner::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => match frame.data_ref() {
                Some(chunk) => {
                    if !self.answered {
                        self.answered = true;
                        emit::first_message(self.method, self.started.elapsed());
                    }
                    self.sent = self
                        .sent
                        .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                    self.messages.feed(chunk);
                }
                None => {
                    if let Some(code) = frame.trailers_ref().and_then(code_of) {
                        self.code = Some(code);
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
        // No status and no clean end: the client went away mid-response.
        let code = self.code.unwrap_or(if self.ended {
            Code::Ok
        } else {
            Code::Cancelled
        });
        emit::call_finished(
            self.method,
            code,
            self.started.elapsed(),
            self.messages.complete,
            self.sent,
        );
    }
}

/// gRPC length-prefixed message header: a compression flag, then the payload
/// length as a big-endian `u32`.
const FRAME_HEADER: usize = 5;

/// Whole gRPC messages counted across chunks that need not split on message
/// boundaries.
#[derive(Debug, Default)]
struct Messages {
    complete: u64,
    /// Payload bytes left in the message being walked.
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
            let [_, a, b, c, d] = self.header;
            match usize::try_from(u32::from_be_bytes([a, b, c, d])) {
                Ok(0) => self.complete += 1,
                Ok(len) => self.owed = len,
                // Only on a 16-bit `usize`; no such payload can be served.
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Messages, FRAME_HEADER};

    fn framed(payload: &[u8]) -> Vec<u8> {
        let len = u32::try_from(payload.len()).expect("a small test payload");
        let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
        frame.push(0);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// Messages, not chunks: one chunk can carry several, and one message can
    /// straddle two chunks.
    #[test]
    fn messages_are_counted_across_chunk_boundaries() {
        let packed: Vec<u8> = [framed(b"aa"), framed(b"bbbb"), framed(b"c")].concat();

        let mut whole = Messages::default();
        whole.feed(&packed);
        assert_eq!(whole.complete, 3);

        let mut split = Messages::default();
        let (head, tail) = packed.split_at(3);
        split.feed(head);
        assert_eq!(split.complete, 0, "nothing is complete inside a header");
        let (middle, rest) = tail.split_at(4);
        split.feed(middle);
        split.feed(rest);
        assert_eq!(split.complete, 3, "same bytes, same count");

        let mut empty = Messages::default();
        empty.feed(&framed(b""));
        assert_eq!(empty.complete, 1, "an empty message is still a message");
    }
}
