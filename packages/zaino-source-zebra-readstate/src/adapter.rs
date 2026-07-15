//! ReadState adapter: implements source traits via Zebra's ReadStateService.

use std::path::Path;

use tower::ServiceExt;
use zebra_chain::parameters::Network;
use zebra_state::{ReadRequest, ReadResponse, ReadStateService};

use zaino_primitives::types::{Block, BlockHash, ChainMetadata, Height};
use zaino_source::{
    FailureMode, FetchError, GetBlockError, GetChainTipError, QueryError,
};

/// Zebra ReadState adapter.
///
/// Holds a read-only [`ReadStateService`] opened against Zebra's
/// finalized state database. Implements source query traits with
/// zero serialization overhead.
pub struct ZebraReadStateAdapter {
    state: ReadStateService,
}

impl ZebraReadStateAdapter {
    /// Open Zebra's state database read-only.
    ///
    /// `cache_dir` is the root Zebra cache directory (e.g. `/var/cache/zebrad-cache`).
    /// The database path is derived from this + the network.
    pub fn open(cache_dir: &Path, network: &Network) -> Result<Self, String> {
        let config = zebra_state::Config {
            cache_dir: cache_dir.to_path_buf(),
            ..Default::default()
        };

        let (state, _db, _sender) = zebra_state::init_read_only(config, network)
            .map_err(|e| format!("failed to open zebra state: {e}"))?;

        Ok(Self { state })
    }
}

impl zaino_source::GetPreIndexCompactBlock for ZebraReadStateAdapter {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::PreIndexCompactBlock, QueryError<GetBlockError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::CompactBlock(zebra_height.into());

        let response = self
            .state
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| FetchError::new(FailureMode::Connection, format!("state service: {e}")))?;

        match response {
            ReadResponse::CompactBlock(Some(compact)) => {
                Ok(zaino_convert_zebra::pre_index_compact_block_from_zebra(&compact))
            }
            ReadResponse::CompactBlock(None) => {
                Err(QueryError::Domain(GetBlockError::HeightNotFound(height)))
            }
            _ => Err(FetchError::new(
                FailureMode::Parse,
                "unexpected response variant".to_string(),
            )
            .into()),
        }
    }
}

impl ZebraReadStateAdapter {
    /// Fetch just the block header — no transaction deserialization at all.
    pub async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<zebra_chain::block::Header, QueryError<GetBlockError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::BlockHeader(zebra_height.into());

        let response = self
            .state
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| FetchError::new(FailureMode::Connection, format!("state service: {e}")))?;

        match response {
            ReadResponse::BlockHeader { header, .. } => Ok((*header).clone()),
            _ => Err(FetchError::new(
                FailureMode::Parse,
                "unexpected response variant".to_string(),
            )
            .into()),
        }
    }

}

impl zaino_source::GetBlock for ZebraReadStateAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_block(
        &self,
        height: Height,
    ) -> Result<Block, QueryError<GetBlockError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::Block(zebra_height.into());

        let response = self
            .state
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| FetchError::new(FailureMode::Connection, format!("state service: {e}")))?;

        match response {
            ReadResponse::Block(Some(arc_block)) => {
                // Convert from &Block — no clone of the Arc'd block.
                zaino_convert_zebra::block_from_zebra(&arc_block, 0, 0)
                    .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()).into())
            }
            ReadResponse::Block(None) => {
                Err(QueryError::Domain(GetBlockError::HeightNotFound(height)))
            }
            _ => Err(FetchError::new(
                FailureMode::Parse,
                "unexpected response variant".to_string(),
            )
            .into()),
        }
    }
}

impl zaino_source::GetChainTip for ZebraReadStateAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    async fn get_chain_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let response = self
            .state
            .clone()
            .oneshot(ReadRequest::Tip)
            .await
            .map_err(|e| FetchError::new(FailureMode::Connection, format!("state service: {e}")))?;

        match response {
            ReadResponse::Tip(Some((height, hash))) => {
                let h = Height::try_from(height.0)
                    .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()))?;
                Ok((BlockHash::from(hash.0), h))
            }
            ReadResponse::Tip(None) => {
                Err(QueryError::Domain(GetChainTipError::NotReady))
            }
            _ => Err(FetchError::new(
                FailureMode::Parse,
                "unexpected response variant".to_string(),
            )
            .into()),
        }
    }
}
