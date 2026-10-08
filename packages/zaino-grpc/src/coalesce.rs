//! Back-to-back ready data chunks joined into one body frame
//!
//! - hyper writes each body chunk as its own DATA frame(s): one record per chunk = a flood of
//!   small frames
//! - h2 >= 0.4.16 receivers charge every DATA frame < 256 B against a connection budget until read
//!   → `GOAWAY too_many_data_frames` (lightwalletd #593)
//! - never waits for more: an inner `Pending` or end flushes what is held (no added latency)

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};

/// One full-size DATA frame (h2's default `SETTINGS_MAX_FRAME_SIZE`)
const TARGET: usize = 16 * 1024;

/// `inner` with every run of ready data chunks below [`TARGET`] joined (a chunk at or over it =
/// passed through uncopied; `next` = what was polled past the held run)
#[derive(Debug)]
pub(crate) struct Coalesced<B: Body> {
    inner: B,
    held: Vec<Bytes>,
    held_len: usize,
    next: Option<Result<Frame<Bytes>, B::Error>>,
    ended: bool,
}

impl<B: Body<Data = Bytes>> Coalesced<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self { inner, held: Vec::new(), held_len: 0, next: None, ended: false }
    }

    /// What is held as one chunk (one held = itself, uncopied)
    fn flush(&mut self) -> Frame<Bytes> {
        let len = std::mem::take(&mut self.held_len);
        if self.held.len() == 1 {
            return Frame::data(self.held.remove(0));
        }
        let mut joined = Vec::with_capacity(len);
        self.held.drain(..).for_each(|chunk| joined.extend_from_slice(&chunk));
        Frame::data(Bytes::from(joined))
    }

    /// `frame` behind whatever is held (order kept: held data first)
    fn behind_held(
        &mut self,
        frame: Result<Frame<Bytes>, B::Error>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        if self.held.is_empty() {
            return Poll::Ready(Some(frame));
        }
        self.next = Some(frame);
        Poll::Ready(Some(Ok(self.flush())))
    }
}

impl<B> Body for Coalesced<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if let Some(next) = this.next.take() {
            return Poll::Ready(Some(next));
        }
        while !this.ended {
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.len() >= TARGET => {
                        return this.behind_held(Ok(Frame::data(data)))
                    }
                    Ok(data) => {
                        this.held_len += data.len();
                        this.held.push(data);
                        if this.held_len >= TARGET {
                            return Poll::Ready(Some(Ok(this.flush())));
                        }
                    }
                    Err(frame) => return this.behind_held(Ok(frame)),
                },
                Poll::Ready(Some(Err(error))) => return this.behind_held(Err(error)),
                Poll::Ready(None) => this.ended = true,
                Poll::Pending if this.held.is_empty() => return Poll::Pending,
                Poll::Pending => return Poll::Ready(Some(Ok(this.flush()))),
            }
        }
        match this.held.is_empty() {
            true => Poll::Ready(None),
            false => Poll::Ready(Some(Ok(this.flush()))),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.held.is_empty() && self.next.is_none() && (self.ended || self.inner.is_end_stream())
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use http::HeaderMap;
    use http_body_util::{BodyExt as _, StreamBody};

    use super::*;

    /// 170 × 100 B ready, a `Pending`, 2 × 100 B, one chunk of `TARGET` B, 100 B, trailers:
    /// - a ready run joins until it reaches `TARGET` (164 × 100 B), the rest flushes at `Pending`
    /// - a chunk at `TARGET` passes uncopied, after what was held; trailers last, after held data
    #[tokio::test]
    async fn ready_small_chunks_join_and_a_pending_inner_or_a_big_chunk_flushes_them() {
        let small = || Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from(vec![1; 100])));
        let big = Bytes::from(vec![2; TARGET]);
        let run = futures::stream::iter((0..170).map(|_| small()));
        let after_a_pending = futures::stream::once(async move {
            tokio::task::yield_now().await;
            small()
        });
        let tail =
            [small(), Ok(Frame::data(big.clone())), small(), Ok(Frame::trailers(HeaderMap::new()))];
        let inner = run.chain(after_a_pending).chain(futures::stream::iter(tail));
        let inner = Box::pin(StreamBody::new(inner));
        let mut body = std::pin::pin!(Coalesced::new(inner));

        let mut frames = Vec::new();
        while let Some(frame) = body.frame().await {
            match frame.expect("infallible").into_data() {
                Ok(data) => frames.push(("data", data.len(), data.as_ptr() == big.as_ptr())),
                Err(_) => frames.push(("trailers", 0, false)),
            }
        }
        let expected = [
            ("data", 16_400, false),
            ("data", 600, false),
            ("data", 200, false),
            ("data", TARGET, true),
            ("data", 100, false),
            ("trailers", 0, false),
        ];
        assert_eq!(frames, expected);
    }
}
