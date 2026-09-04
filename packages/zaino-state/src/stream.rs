//! Holds streaming response types.

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
