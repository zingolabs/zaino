//! ChainIndex's side of the ChainHead boundary: how ChainIndex hands ChainHead
//! a validator, and starts it.

use std::sync::Arc;

use crate::chain_index::{
    source::BlockchainSource, source_ports::ChainIndexSourcePorts,
    validator_source::ValidatorSource,
};
use zaino_chain_head::ChainHeadBlockSource;

/// A source that can also answer ChainHead's questions.
///
/// ChainHead speaks the `zaino-source` ports directly, while ChainIndex still
/// consumes the wire-typed [`BlockchainSource`] scaffolding. This trait is how
/// the second hands over the first: an implementor exposes the underlying
/// validator, and ChainHead is built on that rather than on the wrapper.
///
/// Kept off `BlockchainSource` because that port is frozen scaffolding
/// (docs/adr/zaino/0008) and shrinks as each subsystem moves onto the real ports.
pub trait WithChainHeadSource: BlockchainSource {
    /// The validator ChainHead will drive.
    type Head: ChainHeadBlockSource;

    /// The underlying validator, shared rather than cloned.
    fn chain_head_source(&self) -> Arc<Self::Head>;
}

/// A `ValidatorSource` offers a ChainHead source exactly when the validator it
/// wraps can answer ChainHead's questions.
///
/// The second bound is not redundant with the first: `ChainIndexSourcePorts`
/// names what ChainIndex asks — which includes the *raw* block ports, because
/// it builds its own index from the bytes — while ChainHead asks for parsed
/// blocks. Requiring both here keeps each bound describing one consumer's
/// needs, rather than restating ChainHead's inside ChainIndex's.
impl<V> WithChainHeadSource for ValidatorSource<V>
where
    V: ChainIndexSourcePorts + ChainHeadBlockSource,
{
    type Head = V;

    fn chain_head_source(&self) -> Arc<Self::Head> {
        self.validator()
    }
}

/// The chain head's configuration, retaining the operational window.
pub(super) fn config() -> zaino_chain_head::ChainHeadConfig {
    zaino_chain_head::ChainHeadConfig::with_max_depth(
        std::num::NonZeroU32::new(super::OPERATIONAL_NFS_DEPTH)
            .expect("the operational chain-head depth derives from a non-zero reorg bound"),
    )
}

/// Starts the chain head over `source`.
///
/// Builds a complete window before returning, so every snapshot taken after
/// has one to answer from.
pub(super) async fn spawn<S: WithChainHeadSource>(
    source: &S,
    config: zaino_chain_head::ChainHeadConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<
    Arc<zaino_chain_head_service::ChainHeadService<S::Head>>,
    zaino_chain_head_service::ChainHeadInitError,
> {
    zaino_chain_head_service::ChainHeadService::spawn(source.chain_head_source(), config, cancel)
        .await
}
