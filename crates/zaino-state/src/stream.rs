//! Holds streaming response types.

use tokio_stream::wrappers::ReceiverStream;
use zaino_proto::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{Address, GetAddressUtxosReply, RawTransaction, SubtreeRoot},
};

/// Stream of RawTransactions, output type of get_taddress_txids.
#[derive(Debug)]
pub struct RawTransactionStream {
    inner: ReceiverStream<Result<RawTransaction, tonic::Status>>,
}

impl RawTransactionStream {
    /// Returns new instance of RawTransactionStream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<RawTransaction, tonic::Status>>) -> Self {
        RawTransactionStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for RawTransactionStream {
    type Item = Result<RawTransaction, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(raw_tx))) => std::task::Poll::Ready(Some(Ok(raw_tx))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Stream of RawTransactions, output type of get_taddress_txids.
pub struct CompactTransactionStream {
    inner: ReceiverStream<Result<CompactTx, tonic::Status>>,
}

impl CompactTransactionStream {
    /// Returns new instance of RawTransactionStream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<CompactTx, tonic::Status>>) -> Self {
        CompactTransactionStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for CompactTransactionStream {
    type Item = Result<CompactTx, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(raw_tx))) => std::task::Poll::Ready(Some(Ok(raw_tx))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Stream of CompactBlocks, output type of get_block_range.
pub struct CompactBlockStream {
    inner: ReceiverStream<Result<CompactBlock, tonic::Status>>,
}

impl CompactBlockStream {
    /// Returns new instance of CompactBlockStream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<CompactBlock, tonic::Status>>) -> Self {
        CompactBlockStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for CompactBlockStream {
    type Item = Result<CompactBlock, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(raw_tx))) => std::task::Poll::Ready(Some(Ok(raw_tx))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Stream of CompactBlocks, output type of get_block_range.
pub struct UtxoReplyStream {
    inner: ReceiverStream<Result<GetAddressUtxosReply, tonic::Status>>,
}

impl UtxoReplyStream {
    /// Returns new instance of CompactBlockStream.
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<Result<GetAddressUtxosReply, tonic::Status>>,
    ) -> Self {
        UtxoReplyStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for UtxoReplyStream {
    type Item = Result<GetAddressUtxosReply, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(raw_tx))) => std::task::Poll::Ready(Some(Ok(raw_tx))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Stream of CompactBlocks, output type of get_block_range.
pub struct SubtreeRootReplyStream {
    inner: ReceiverStream<Result<SubtreeRoot, tonic::Status>>,
}

impl SubtreeRootReplyStream {
    /// Returns new instance of CompactBlockStream.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<SubtreeRoot, tonic::Status>>) -> Self {
        SubtreeRootReplyStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for SubtreeRootReplyStream {
    type Item = Result<SubtreeRoot, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(raw_tx))) => std::task::Poll::Ready(Some(Ok(raw_tx))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Stream of `Address`, input type for `get_taddress_balance_stream`.
pub struct AddressStream {
    inner: ReceiverStream<Result<Address, tonic::Status>>,
}

impl AddressStream {
    /// Creates a new `AddressStream` instance.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<Address, tonic::Status>>) -> Self {
        AddressStream {
            inner: ReceiverStream::new(rx),
        }
    }
}

impl futures::Stream for AddressStream {
    type Item = Result<Address, tonic::Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_next(cx);
        match poll {
            std::task::Poll::Ready(Some(Ok(address))) => std::task::Poll::Ready(Some(Ok(address))),
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
