use futures::TryStreamExt;
use thiserror::Error;
use tonic::transport::{Channel, Endpoint};
use tonic::Status;
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
    },
};

#[derive(Debug, Error)]
pub(super) enum Error {
    #[error("failed to connect to {url}: {source}")]
    Connect {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("gRPC call failed: {0}")]
    Grpc(#[from] Status),
}

pub(super) async fn connect_eager(url: &str) -> Result<CompactTxStreamerClient<Channel>, Error> {
    let mut endpoint = endpoint(url)?;
    endpoint = endpoint.keep_alive_while_idle(true);

    let channel = if endpoint.uri().scheme_str() == Some("https") {
        endpoint
            .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())
            .map_err(|source| Error::Connect {
                url: url.to_string(),
                source: Box::new(source),
            })?
            .connect()
            .await
            .map_err(|source| Error::Connect {
                url: url.to_string(),
                source: Box::new(source),
            })?
    } else {
        endpoint.connect().await.map_err(|source| Error::Connect {
            url: url.to_string(),
            source: Box::new(source),
        })?
    };

    Ok(CompactTxStreamerClient::new(channel))
}

pub(super) fn connect_lazy(url: &str) -> Result<CompactTxStreamerClient<Channel>, Error> {
    let endpoint = endpoint(url)?;

    let channel = if endpoint.uri().scheme_str() == Some("https") {
        endpoint
            .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())
            .map_err(|source| Error::Connect {
                url: url.to_string(),
                source: Box::new(source),
            })?
            .connect_lazy()
    } else {
        endpoint.connect_lazy()
    };

    Ok(CompactTxStreamerClient::new(channel))
}

pub(super) async fn get_latest_height(
    client: &mut CompactTxStreamerClient<Channel>,
) -> Result<u64, Error> {
    let response = client.get_latest_block(ChainSpec::default()).await?;
    Ok(response.into_inner().height)
}

pub(super) async fn fetch_block_range(
    client: &mut CompactTxStreamerClient<Channel>,
    start_height: u64,
    end_height: u64,
) -> Result<Vec<CompactBlock>, Error> {
    let response = client
        .get_block_range(BlockRange {
            start: Some(BlockId {
                height: start_height,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: end_height,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        })
        .await?;

    let mut blocks: Vec<CompactBlock> = response.into_inner().try_collect().await?;
    blocks.sort_by_key(|block| block.height);

    Ok(blocks)
}

pub(super) fn copy_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() == 32 {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(bytes);
        Some(hash)
    } else {
        None
    }
}

fn endpoint(url: &str) -> Result<Endpoint, Error> {
    Endpoint::from_shared(url.to_string()).map_err(|source| Error::Connect {
        url: url.to_string(),
        source: Box::new(source),
    })
}
