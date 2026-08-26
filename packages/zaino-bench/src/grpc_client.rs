//! Thin tonic client helpers over `CompactTxStreamer`.
//!
//! The workspace has no reusable public client helper — `zaino-testutils`'
//! `build_client` is `#[cfg(test)]` and `tonic` is only a dev-dependency there —
//! so the load generator carries its own.

use futures::TryStreamExt;
use tonic::transport::{Channel, Endpoint};
use tonic::Status;
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
    },
};

/// A failure reaching, or talking to, the server under test.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The endpoint URL was malformed, or the TCP/TLS connection failed.
    #[error("failed to connect to {url}: {source}")]
    Connect {
        /// The endpoint that was dialled.
        url: String,
        /// The underlying transport failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The server accepted the connection but rejected the call.
    #[error("gRPC call failed: {0}")]
    Grpc(#[from] Status),
}

/// Dials `url` and completes the connection before returning.
///
/// The load generator needs the connect step to be a *measurable* phase, which
/// rules out tonic's lazy channel: a lazy connect would fold the handshake into
/// the first request and inflate the fetch timing instead.
pub(crate) async fn connect_eager(url: &str) -> Result<CompactTxStreamerClient<Channel>, Error> {
    let endpoint = endpoint(url)?.keep_alive_while_idle(true);

    let channel = match endpoint.uri().scheme_str() {
        Some("https") => tls(&endpoint, url)?.connect().await,
        _ => endpoint.connect().await,
    }
    .map_err(|source| connect_error(url, source))?;

    Ok(CompactTxStreamerClient::new(channel))
}

/// Returns the chain tip height the server is currently advertising.
pub(crate) async fn get_latest_height(
    client: &mut CompactTxStreamerClient<Channel>,
) -> Result<u64, Error> {
    let response = client.get_latest_block(ChainSpec::default()).await?;
    Ok(response.into_inner().height)
}

/// Opens a `GetBlockRange` stream over `start..=end`.
///
/// Returns the raw stream rather than a collected `Vec` so a caller measuring
/// serve rate can time each block as it arrives; [`fetch_block_range`] collects
/// for callers that only need the total.
pub(crate) async fn block_range_stream(
    client: &mut CompactTxStreamerClient<Channel>,
    start: u64,
    end: u64,
) -> Result<tonic::Streaming<CompactBlock>, Error> {
    let response = client.get_block_range(block_range(start, end)).await?;
    Ok(response.into_inner())
}

/// Streams `start..=end` to completion, returning the blocks sorted by height.
pub(crate) async fn fetch_block_range(
    client: &mut CompactTxStreamerClient<Channel>,
    start: u64,
    end: u64,
) -> Result<Vec<CompactBlock>, Error> {
    let stream = block_range_stream(client, start, end).await?;
    let mut blocks: Vec<CompactBlock> = stream.try_collect().await?;
    blocks.sort_by_key(|block| block.height);
    Ok(blocks)
}

/// Copies a wire hash into a fixed array, rejecting any other length.
///
/// A wrong length is a protocol violation rather than a chain break, so callers
/// count it separately; returning `Option` keeps that decision at the call site.
pub(crate) fn copy_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).ok()
}

fn block_range(start: u64, end: u64) -> BlockRange {
    BlockRange {
        start: Some(BlockId {
            height: start,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: end,
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    }
}

fn endpoint(url: &str) -> Result<Endpoint, Error> {
    Endpoint::from_shared(url.to_string()).map_err(|source| connect_error(url, source))
}

fn tls(endpoint: &Endpoint, url: &str) -> Result<Endpoint, Error> {
    endpoint
        .clone()
        .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
        .map_err(|source| connect_error(url, source))
}

fn connect_error(url: &str, source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Connect {
        url: url.to_string(),
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_hash_accepts_only_32_bytes() {
        assert_eq!(copy_hash(&[7u8; 32]), Some([7u8; 32]));
        assert_eq!(copy_hash(&[7u8; 31]), None);
        assert_eq!(copy_hash(&[7u8; 33]), None);
        assert_eq!(copy_hash(&[]), None);
    }

    #[test]
    fn block_range_spans_the_requested_heights_unfiltered() {
        let range = block_range(100, 200);
        assert_eq!(range.start.map(|id| id.height), Some(100));
        assert_eq!(range.end.map(|id| id.height), Some(200));
        // An unfiltered request is what real clients send; filtering here would
        // measure a cheaper path than the one under test.
        assert!(range.pool_types.is_empty());
    }
}
