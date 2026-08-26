//! Streamed reads.
//!
//! # Temporary
//!
//! Carries `tonic::Status` as its error type, which is a serving concern with
//! no business in a storage crate: an LMDB cursor desync should not be phrased
//! as a gRPC status. It is here because the compact-block reader that produces
//! it has not yet been moved onto the domain's chunked stream, which carries a
//! `ChainStoreError`. Both go together when it is.

use tokio_stream::wrappers::ReceiverStream;
use zaino_proto::proto::compact_formats::CompactBlock;

/// Watches one server stream's delivery.
///
/// - Implemented in the serving layer: only it knows the gRPC method, and the
///   stream is built where that name does not exist
/// - Its `Drop` marks the stream finished, so a mid-range client hangup — missed
///   by a completion-only hook — still lands
pub trait StreamObserver: Send + std::fmt::Debug {
    /// One item yielded to the client.
    fn item(&mut self);
}

/// A stream of `Result<T, tonic::Status>` items read from a tokio mpsc receiver.
///
/// - `observer` set by the serving layer ([`observed`](Self::observed)); `None`
///   for streams built elsewhere (tests, plumbing), with no method to charge
/// - Ungated: gating it on `prometheus` here breaks `zaino-serve` whenever the two
///   are enabled independently, for one pointer and one predicted null check
#[derive(Debug)]
pub struct ChannelStream<T> {
    inner: ReceiverStream<Result<T, tonic::Status>>,
    observer: Option<Box<dyn StreamObserver>>,
}

impl<T> ChannelStream<T> {
    /// Wraps the receiving half of an mpsc channel as a stream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<T, tonic::Status>>) -> Self {
        ChannelStream {
            inner: ReceiverStream::new(rx),
            observer: None,
        }
    }

    /// Attach `observer` to measure delivery.
    ///
    /// - Consumes & returns `Self` so the service's associated stream types stay
    ///   put; an adapter wrapper would change every `type Get*Stream` and ripple
    ///   into the trait
    pub fn observed(mut self, observer: Box<dyn StreamObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

impl<T> futures::Stream for ChannelStream<T> {
    type Item = Result<T, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        // On delivery, not production: the question is what the client received,
        // and an abandoned stream produced far more than it delivered
        if let std::task::Poll::Ready(Some(Ok(_))) = &polled {
            if let Some(observer) = self.observer.as_mut() {
                observer.item();
            }
        }
        polled
    }
}

/// Stream of `CompactBlock` items, output type of get_block_range.
pub type CompactBlockStream = ChannelStream<CompactBlock>;
