//! Holds streaming response types.

use std::pin::Pin;

use futures::{Stream, StreamExt as _};
use zaino_primitives::types::{BlockRef, ChainStateEpoch};

use zaino_proto::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{Address, GetAddressUtxosReply, RawTransaction, SubtreeRoot},
};

/// A stream of `Result<T, tonic::Status>` items read from a tokio mpsc channel.
///
/// # Temporary home
///
/// Defined in `zaino-chain-store-zainodb` and re-exported here, which is the
/// wrong way round: this is a serving type and it has no business in a storage
/// crate. It is there because the finalised store's legacy compact-block stream
/// still returns one, and two structurally identical types would mean a
/// pointless conversion at that seam. The definition comes back here when that
/// method is deleted.
pub use zaino_chain_store_zainodb::stream::ChannelStream;

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

/// A latest-state stream of locally indexed canonical tips.
pub type IndexedTipStream = Pin<Box<dyn Stream<Item = BlockRef> + Send>>;

pub(crate) fn indexed_tip_stream(
    mut updates: tokio::sync::watch::Receiver<ChainStateEpoch>,
) -> IndexedTipStream {
    let initial = updates.borrow_and_update().best_tip;
    let changes = futures::stream::unfold(updates, |mut updates| async move {
        updates.changed().await.ok()?;
        let tip = updates.borrow_and_update().best_tip;
        Some((tip, updates))
    });

    Box::pin(futures::stream::once(async move { initial }).chain(changes))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use zaino_primitives::types::{BlockHash, BlockRef, ChainStateEpoch, Height};

    use super::indexed_tip_stream;

    fn epoch(generation: u64, height: u32, hash_byte: u8) -> ChainStateEpoch {
        ChainStateEpoch {
            generation,
            best_tip: BlockRef {
                hash: BlockHash::from([hash_byte; 32]),
                height: Height::try_from(height).expect("test height is within the protocol limit"),
            },
        }
    }

    #[tokio::test]
    async fn indexed_tip_stream_emits_initial_snapshot_before_updates() {
        // Given
        let (sender, receiver) = tokio::sync::watch::channel(epoch(0, 10, 1));
        let mut stream = indexed_tip_stream(receiver);

        // When
        sender.send_replace(epoch(1, 11, 2));

        // Then
        assert_eq!(stream.next().await, Some(epoch(0, 10, 1).best_tip));
        assert_eq!(stream.next().await, Some(epoch(1, 11, 2).best_tip));
    }

    #[tokio::test]
    async fn indexed_tip_stream_coalesces_updates_to_latest_readable_tip() {
        // Given
        let (sender, receiver) = tokio::sync::watch::channel(epoch(0, 10, 1));
        let mut stream = indexed_tip_stream(receiver);
        assert_eq!(stream.next().await, Some(epoch(0, 10, 1).best_tip));

        // When
        sender.send_replace(epoch(1, 11, 2));
        sender.send_replace(epoch(2, 12, 3));

        // Then
        assert_eq!(stream.next().await, Some(epoch(2, 12, 3).best_tip));
    }

    #[tokio::test]
    async fn indexed_tip_stream_emits_same_height_reorg() {
        // Given
        let (sender, receiver) = tokio::sync::watch::channel(epoch(0, 12, 3));
        let mut stream = indexed_tip_stream(receiver);
        assert_eq!(stream.next().await, Some(epoch(0, 12, 3).best_tip));

        // When
        sender.send_replace(epoch(1, 12, 4));

        // Then
        assert_eq!(stream.next().await, Some(epoch(1, 12, 4).best_tip));
    }

    #[test]
    fn dropping_indexed_tip_stream_releases_subscription() {
        // Given
        let (sender, receiver) = tokio::sync::watch::channel(epoch(0, 10, 1));
        let stream = indexed_tip_stream(receiver);
        assert_eq!(sender.receiver_count(), 1);

        // When
        drop(stream);

        // Then
        assert_eq!(sender.receiver_count(), 0);
    }
}
