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

/// A stream of `Result<T, tonic::Status>` items read from a tokio mpsc receiver.
#[derive(Debug)]
pub struct ChannelStream<T> {
    inner: ReceiverStream<Result<T, tonic::Status>>,
}

impl<T> ChannelStream<T> {
    /// Wraps the receiving half of an mpsc channel as a stream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<T, tonic::Status>>) -> Self {
        ChannelStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl<T> futures::Stream for ChannelStream<T> {
    type Item = Result<T, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Stream of `CompactBlock` items, output type of get_block_range.
pub type CompactBlockStream = ChannelStream<CompactBlock>;
