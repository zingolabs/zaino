//! Holds streaming response types.

use tokio_stream::wrappers::ReceiverStream;
use zaino_proto::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{Address, GetAddressUtxosReply, RawTransaction, SubtreeRoot},
};

/// Watches one server stream's delivery.
///
/// - Implemented in the serving layer: only it knows the gRPC method, and the
///   stream is built here where that name does not exist
/// - Its `Drop` marks the stream finished, so a mid-range client hangup — missed
///   by a completion-only hook — still lands
pub trait StreamObserver: Send + std::fmt::Debug {
    /// One item yielded to the client.
    fn item(&mut self);
}

/// A stream of `Result<T, tonic::Status>` items read from a tokio mpsc receiver.
#[derive(Debug)]
pub struct ChannelStream<T> {
    inner: ReceiverStream<Result<T, tonic::Status>>,
    /// Set by the serving layer via [`ChannelStream::observed`]; `None` for
    /// streams built elsewhere (tests, plumbing), with no method to charge.
    ///
    /// - Ungated: the observer is gated on `zaino-serve`'s feature, so gating here
    ///   too breaks its build whenever the two are enabled independently — a
    ///   unification trap bought for one pointer and one predicted null check
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

/// Stream of `RawTransaction` items, output type of get_taddress_txids.
pub type RawTransactionStream = ChannelStream<RawTransaction>;

/// Stream of `CompactTx` items, output type of get_mempool_tx.
pub type CompactTransactionStream = ChannelStream<CompactTx>;

/// Stream of `CompactBlock` items, output type of get_block_range.
pub type CompactBlockStream = ChannelStream<CompactBlock>;

/// Stream of `GetAddressUtxosReply` items, output type of get_address_utxos_stream.
pub type UtxoReplyStream = ChannelStream<GetAddressUtxosReply>;

/// Stream of `SubtreeRoot` items, output type of get_subtree_roots.
pub type SubtreeRootReplyStream = ChannelStream<SubtreeRoot>;

/// Stream of `Address` items, input type for get_taddress_balance_stream.
pub type AddressStream = ChannelStream<Address>;
