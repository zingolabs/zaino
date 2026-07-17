//! Holds streaming response types.

use tokio_stream::wrappers::ReceiverStream;
use zaino_proto::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{Address, GetAddressUtxosReply, RawTransaction, SubtreeRoot},
};

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
