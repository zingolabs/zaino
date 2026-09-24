//! ReadState adapter: implements source traits via Zebra's ReadStateService.
//!
//! # Capabilities this adapter does not claim
//!
//! Each trait here answers one question with one read of the state service.
//! Two questions are deliberately left unimplemented because they cannot be
//! answered that way, and pretending otherwise would either duplicate logic or
//! silently return less than the caller asked for:
//!
//! - **`GetAddressDeltas`** is a composition: address txids, then every
//!   transaction, then a derivation over them. Crucially the transaction step
//!   needs the mempool, which the state service does not have — an
//!   implementation here would quietly omit unconfirmed deltas. It belongs
//!   above the adapters, where both transports are in reach.
//! - **`SubscribeChainTip`** needs a `ChainTipChange`, which the read-only
//!   construction path does not produce (see the impl below).
//!
//! Leaving them out is the capability model working: a composite routes each to
//! whichever transport can answer it, rather than every adapter growing a
//! partial version.
//!
//! `GetBlockDeltas` *is* implemented here, and was once on this list on the
//! grounds that the validator computes it in one call. That was wrong: zebrad
//! does not implement `getblockdeltas` at all, so the derivation below is the
//! only implementation a zebrad-backed deployment has.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tower::ServiceExt;
use zebra_chain::parameters::Network;
use zebra_state::{ReadRequest, ReadResponse, ReadStateService, ZebraDb};

use zaino_async::{run_blocking, TaskName};
use zaino_primitives::types::{
    Block, BlockHash, ChainMetadata, Height, TreeRoot, TreeRootInfo, TreeSize,
};
use zaino_source::{FailureMode, GetBlockError, GetChainTipError, NonDomainError, QueryError};

/// The readstate adapter's own transport-fault vocabulary.
///
/// Foreign causes (the state service's error, decode failures) are captured here
/// with the adapter's local context, then mapped to the shared seam by one
/// deterministic `From<ReadStateError> for NonDomainError`. Lower deps never appear in
/// this adapter's public signatures — only as `#[source]` causes inside this type.
#[derive(Debug, thiserror::Error)]
pub enum ReadStateError {
    /// The state service could not be reached or failed to answer.
    #[error("state service unavailable")]
    Unreachable(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The state service's bytes could not be decoded into the domain type.
    #[error("invalid data from the state service")]
    InvalidData(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The state service answered off-contract (wrong variant, or rows out of order).
    #[error("state service answered off-contract: {0}")]
    OffContract(String),
}

impl ReadStateError {
    fn unreachable(cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
        Self::Unreachable(cause.into())
    }
    fn invalid_data(cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
        Self::InvalidData(cause.into())
    }
    fn off_contract(what: impl Into<String>) -> Self {
        Self::OffContract(what.into())
    }
}

impl From<ReadStateError> for NonDomainError {
    fn from(e: ReadStateError) -> Self {
        let mode = match &e {
            ReadStateError::Unreachable(_) => FailureMode::Connection,
            // Corrupt in-process data is not a wire parse; classified `Parse`
            // (non-retryable) for now — a dedicated `FailureMode::InvalidSourceData`
            // waits for a consumer that branches on it.
            ReadStateError::InvalidData(_) | ReadStateError::OffContract(_) => FailureMode::Parse,
        };
        NonDomainError::from_cause(mode, e)
    }
}

impl zaino_source::ValidatorSource for ZebraReadStateAdapter {
    // This adapter owns a distinct non-domain vocabulary; `ReadStateError` maps to
    // the seam via the deterministic `From<ReadStateError> for NonDomainError`.
    type NonDomain = ReadStateError;
}

impl<E: std::fmt::Debug + std::fmt::Display> From<ReadStateError>
    for QueryError<E, ReadStateError>
{
    fn from(e: ReadStateError) -> Self {
        // The adapter's own type rides in the non-domain slot; the seam
        // conversion happens later, once, in the resilience wrapper.
        QueryError::NonDomain(e)
    }
}

/// Opening the read-only state database failed.
///
/// The underlying zebra cause rides as a `#[source]`, never in the public
/// signature — the same discipline [`ReadStateError`] applies to query faults.
#[derive(Debug, thiserror::Error)]
#[error("failed to open the zebra state database")]
pub struct OpenReadStateError(#[source] Box<dyn std::error::Error + Send + Sync + 'static>);

/// How stale the read-only secondary may get before the next read refreshes it.
///
/// The secondary observes newly-finalized blocks only after a catch-up, so reads
/// refresh it lazily at most this often: a burst of reads pays for at most one
/// catch-up, and the finalized view is never more than this interval behind the
/// primary. The chain *tip* is served over JSON-RPC by the composite, not from
/// here, so this bounds finalized-history freshness only, never the tip.
const CATCH_UP_INTERVAL: Duration = Duration::from_secs(1);

/// The state service answered with a variant that does not correspond to the
/// request.
///
/// An error rather than a panic. This is a library: a state service that
/// answers off-contract is a reason to fail the query, not to take the process
/// down, and the previous implementation's `unreachable!` did the latter.
fn unexpected_response(request: &'static str) -> ReadStateError {
    ReadStateError::off_contract(format!("unexpected response to {request}"))
}

/// A pool's tree root and its note count as the state service reports it.
///
/// The count arrives as a `u64`; one the compact protocol cannot carry is
/// refused here rather than narrowed.
fn tree_root_info(root: [u8; 32], count: u64) -> Result<TreeRootInfo, ReadStateError> {
    let size = TreeSize::try_from(count).map_err(ReadStateError::invalid_data)?;
    Ok(TreeRootInfo {
        root: TreeRoot::new(root),
        size,
    })
}

/// The state service returned rows out of order.
///
/// Zebra's address indexes are documented as ordered, and Zaino relies on that
/// to stream results without sorting. The previous implementation asserted it,
/// which turns a misbehaving or corrupted index into a process abort; a caller
/// can do something useful with an error.
fn out_of_order(index: &'static str) -> ReadStateError {
    ReadStateError::off_contract(format!("{index} rows out of chain order"))
}

/// Zebra ReadState adapter.
///
/// Holds a read-only [`ReadStateService`] opened against Zebra's
/// finalized state database. Implements source query traits with
/// zero serialization overhead.
pub struct ZebraReadStateAdapter {
    state: ReadStateService,

    /// The syncer feeding `state`, when this adapter launched one.
    ///
    /// Held so shutdown can stop it: a syncer outliving the indexer would keep
    /// writing to a database nothing is reading. `None` when the service was
    /// opened read-only, which starts no syncer of its own.
    syncer: Option<std::sync::Arc<tokio::task::JoinHandle<()>>>,
    /// Retained because difficulty is network-relative: the same threshold
    /// means a different multiple of the minimum on each network, so it cannot
    /// be computed from the tip alone.
    network: Network,
    /// The finalized-state handle for a read-only secondary opened by
    /// [`open`](Self::open), used to catch it up to the primary's newly-committed
    /// blocks. `None` for a caller-launched service ([`from_service`](Self::from_service)),
    /// which follows the primary through its own syncer and needs no manual
    /// catch-up.
    db: Option<ZebraDb>,
    /// When the secondary was last caught up, throttling catch-up across
    /// concurrent reads (see [`maybe_catch_up`](Self::maybe_catch_up)).
    last_caught_up: Mutex<Instant>,
}

impl ZebraReadStateAdapter {
    /// Ask the state service one question, refreshing the read-only secondary
    /// first if a refresh is due (see [`maybe_catch_up`](Self::maybe_catch_up)).
    ///
    /// Every read repeats the same steps — refresh, clone the service, await,
    /// and turn a service failure into a transport error — so they live here
    /// once. The caller matches the response variant, which is the only part
    /// that genuinely differs.
    async fn read(&self, request: ReadRequest) -> Result<ReadResponse, ReadStateError> {
        self.maybe_catch_up().await;
        self.state
            .clone()
            .oneshot(request)
            .await
            .map_err(ReadStateError::unreachable)
    }

    /// Catch the read-only secondary up to the primary's latest finalized state,
    /// throttled to [`CATCH_UP_INTERVAL`] and skipped for a caller-launched
    /// service (which follows the primary through its own syncer).
    ///
    /// Best-effort: a failed catch-up leaves the secondary on its current view
    /// and the read proceeds — the next interval retries. "Slightly behind" is
    /// not "unavailable", so this is never surfaced as a read error.
    async fn maybe_catch_up(&self) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let due = {
            // The guard protects only a timestamp swap that cannot panic, so a
            // poison means an unrelated thread already crashed; recover the
            // timestamp and carry on rather than propagate — a refresh is
            // best-effort and has no failure to bubble to the read.
            let mut last = self
                .last_caught_up
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.elapsed() >= CATCH_UP_INTERVAL {
                // Stamp before the blocking catch-up so concurrent reads within
                // the interval skip it — one refresher per interval.
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if due {
            // Blocking RocksDB I/O: run it off the async workers through the
            // async layer's blocking primitive, which renders a panic as a named
            // error rather than leaking a raw tokio JoinError.
            match run_blocking(TaskName("readstate-catch-up"), move || {
                db.try_catch_up_with_primary()
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(catch_up_err)) => {
                    let _ = &catch_up_err;
                    #[cfg(feature = "tracing")]
                    tracing::debug!(
                        error = %catch_up_err,
                        "read-state secondary catch-up failed; serving current view"
                    );
                }
                Err(task_err) => {
                    let _ = &task_err;
                    #[cfg(feature = "tracing")]
                    tracing::debug!(
                        error = %task_err,
                        "read-state secondary catch-up task did not run; serving current view"
                    );
                }
            }
        }
    }
}

impl ZebraReadStateAdapter {
    /// Open Zebra's state database read-only.
    ///
    /// `cache_dir` is the root Zebra cache directory (e.g. `/var/cache/zebrad-cache`).
    /// The database path is derived from this + the network.
    ///
    /// The handle is a RocksDB *secondary* that follows the primary (a running
    /// zebrad): it is a point-in-time snapshot until caught up. Reads refresh it
    /// lazily — see [`read`](Self::read) and [`maybe_catch_up`](Self::maybe_catch_up) —
    /// so the finalized view tracks the primary without a background task. The
    /// non-finalized top of the chain is served elsewhere (JSON-RPC), so the
    /// read-only construction's caller-fed non-finalized channel is not needed
    /// here and is dropped.
    pub fn open(cache_dir: &Path, network: &Network) -> Result<Self, OpenReadStateError> {
        let config = zebra_state::Config {
            cache_dir: cache_dir.to_path_buf(),
            ..Default::default()
        };

        let (state, db, _sender) = zebra_state::init_read_only(config, network)
            .map_err(|e| OpenReadStateError(Box::new(e)))?;

        Ok(Self {
            state,
            syncer: None,
            network: network.clone(),
            db: Some(db),
            last_caught_up: Mutex::new(Instant::now()),
        })
    }
}

impl ZebraReadStateAdapter {
    /// Wrap a `ReadStateService` that a caller has already launched.
    ///
    /// The read-only [`open`](Self::open) path starts no syncer and so has no
    /// tip stream; a caller that needs one launches the service itself and
    /// hands it here, along with the syncer task so this adapter can stop it.
    pub fn from_service(
        state: ReadStateService,
        network: &Network,
        syncer: Option<std::sync::Arc<tokio::task::JoinHandle<()>>>,
    ) -> Self {
        Self {
            state,
            syncer,
            network: network.clone(),
            // A caller-launched service follows the primary through its own
            // syncer, so there is no secondary to catch up.
            db: None,
            last_caught_up: Mutex::new(Instant::now()),
        }
    }
}

#[cfg(feature = "test_dependencies")]
impl ZebraReadStateAdapter {
    /// The underlying service.
    ///
    /// Test-only escape hatch: live tests recompute expected chain data
    /// straight off the state service. Production code goes through the ports.
    pub fn read_state_service(&self) -> &ReadStateService {
        &self.state
    }
}

impl zaino_source::OneShotGetPreIndexCompactBlock for ZebraReadStateAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<
        zaino_primitives::types::PreIndexCompactBlock,
        QueryError<GetBlockError, ReadStateError>,
    > {
        // The fork's `ReadRequest::CompactBlock` serves a zaino-shaped compact
        // block (transparent + shielded, proofs/signatures skipped), so the
        // indexer never pays to deserialize proofs it does not read — the point
        // of the compact path, and where the sandblast-era win lives.
        let compact = self.get_compact_block(height).await?;
        zaino_convert_zebra::pre_index_compact_block_from_zebra(&compact)
            .map_err(|e| ReadStateError::invalid_data(e).into())
    }
}

impl ZebraReadStateAdapter {
    /// Fetch a compact block via the fork's `ReadRequest::CompactBlock` — the
    /// zaino-shaped block (transparent outpoints + outputs and shielded
    /// spends/outputs/actions) with proofs, signatures, and input scripts never
    /// deserialized.
    ///
    /// Despite the fork type's name, this is semantically a **pre-index** compact
    /// block: it carries no [`ChainMetadata`] (commitment-tree sizes), which is
    /// indexed state the source cannot know. That is why the domain type it
    /// converts into is `PreIndexCompactBlock`, not the serving `CompactBlock` —
    /// the conversion lives in
    /// [`zaino_convert_zebra::pre_index_compact_block_from_zebra`].
    pub async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<
        zebra_chain::transaction::compact::CompactBlock,
        QueryError<GetBlockError, ReadStateError>,
    > {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::CompactBlock(zebra_height.into());

        let ReadResponse::CompactBlock(compact) = self.read(request).await? else {
            return Err(unexpected_response("CompactBlock").into());
        };
        match compact {
            Some(compact) => Ok(compact),
            None => Err(QueryError::Domain(GetBlockError::HeightNotFound(height))),
        }
    }

    /// Fetch just the block header — no transaction deserialization at all.
    pub async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<zebra_chain::block::Header, QueryError<GetBlockError, ReadStateError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::BlockHeader(zebra_height.into());

        let ReadResponse::BlockHeader { header, .. } = self.read(request).await? else {
            return Err(unexpected_response("BlockHeader").into());
        };
        Ok(*header)
    }
}

impl zaino_source::OneShotGetBlock for ZebraReadStateAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_block(
        &self,
        height: Height,
    ) -> Result<Block, QueryError<GetBlockError, ReadStateError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));
        let request = ReadRequest::Block(zebra_height.into());

        let ReadResponse::Block(block) = self.read(request).await? else {
            return Err(unexpected_response("Block").into());
        };
        match block {
            Some(arc_block) => {
                // Convert from &Block — no clone of the Arc'd block.
                //
                // Cumulative tree sizes are indexed state rather than block
                // data, so they are zero here and filled in by whatever tracks
                // them. Zero is a placeholder, not a measurement.
                let chain_metadata = ChainMetadata::ZERO;
                zaino_convert_zebra::block_from_zebra(&arc_block, chain_metadata)
                    .map_err(|e| ReadStateError::invalid_data(e).into())
            }
            None => Err(QueryError::Domain(GetBlockError::HeightNotFound(height))),
        }
    }
}

impl zaino_source::OneShotGetChainTip for ZebraReadStateAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    async fn get_chain_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<GetChainTipError, ReadStateError>> {
        let ReadResponse::Tip(tip) = self.read(ReadRequest::Tip).await? else {
            return Err(unexpected_response("Tip").into());
        };
        match tip {
            Some((height, hash)) => {
                let h = Height::try_from(height.0).map_err(ReadStateError::invalid_data)?;
                Ok((BlockHash::from(hash.0), h))
            }
            None => Err(QueryError::Domain(GetChainTipError::NotReady)),
        }
    }
}

impl zaino_source::OneShotGetBlockByHash for ZebraReadStateAdapter {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<zaino_source::GetBlockByHashError, ReadStateError>> {
        let zebra_hash = zebra_chain::block::Hash(hash.into());

        let ReadResponse::Block(block) = self.read(ReadRequest::Block(zebra_hash.into())).await?
        else {
            return Err(unexpected_response("Block").into());
        };
        match block {
            Some(arc_block) => {
                // Tree sizes are indexed state, not block data — see `GetBlock`.
                let chain_metadata = ChainMetadata::ZERO;
                zaino_convert_zebra::block_from_zebra(&arc_block, chain_metadata)
                    .map_err(|e| ReadStateError::invalid_data(e).into())
            }
            // Zebra's read-state does not serve side-chain blocks, so an absent
            // block here means "not in the finalized state" rather than "no such
            // block anywhere". A composite that also has an RPC adapter should
            // fall back to it before concluding the block does not exist.
            None => Err(QueryError::Domain(
                zaino_source::GetBlockByHashError::NotFound(hash),
            )),
        }
    }
}

impl zaino_source::OneShotGetBestBlockHeight for ZebraReadStateAdapter {
    async fn get_best_block_height(
        &self,
    ) -> Result<Height, QueryError<zaino_source::GetBestBlockHeightError, ReadStateError>> {
        let ReadResponse::Tip(tip) = self.read(ReadRequest::Tip).await? else {
            return Err(unexpected_response("Tip").into());
        };
        match tip {
            Some((height, _hash)) => {
                Height::try_from(height.0).map_err(|e| ReadStateError::invalid_data(e).into())
            }
            // A composite holding both adapters routes the fallback when the
            // finalized state has no tip; this adapter answers only for its own
            // transport, keeping "what this transport can answer" separate from
            // "what to do when it cannot".
            None => Err(QueryError::Domain(
                zaino_source::GetBestBlockHeightError::NotReady,
            )),
        }
    }
}

impl zaino_source::OneShotGetSubtreeRoots for ZebraReadStateAdapter {
    async fn get_subtree_roots(
        &self,
        pool: zaino_primitives::types::ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<
        Vec<zaino_primitives::types::SubtreeRoot>,
        QueryError<zaino_source::GetSubtreeRootsError, ReadStateError>,
    > {
        use zaino_primitives::types::{ShieldedPool, SubtreeRoot, TreeRoot};
        use zebra_chain::subtree::NoteCommitmentSubtreeIndex;

        let start_index = NoteCommitmentSubtreeIndex(start_index);
        let limit = limit.map(NoteCommitmentSubtreeIndex);

        let request = match pool {
            ShieldedPool::Sapling => ReadRequest::SaplingSubtrees { start_index, limit },
            ShieldedPool::Orchard => ReadRequest::OrchardSubtrees { start_index, limit },
            ShieldedPool::Ironwood => ReadRequest::IronwoodSubtrees { start_index, limit },
        };

        let response = self.read(request).await?;

        // Each pool answers with its own response variant, so the match is on
        // the pair. Sapling roots serialise via `to_bytes`; Orchard and
        // Ironwood share a representation and use `to_repr`.
        // An out-of-range end height is propagated rather than defaulted:
        // substituting a placeholder would put a subtree at the wrong point in
        // the chain, which is worse than failing the query.
        let roots: Result<Vec<_>, ReadStateError> = match (pool, response) {
            (ShieldedPool::Sapling, ReadResponse::SaplingSubtrees(subtrees)) => subtrees
                .values()
                .map(|subtree| {
                    Ok(SubtreeRoot {
                        root: TreeRoot::new(subtree.root.to_bytes()),
                        end_height: subtree_end_height(subtree.end_height)?,
                    })
                })
                .collect(),
            (ShieldedPool::Orchard, ReadResponse::OrchardSubtrees(subtrees))
            | (ShieldedPool::Ironwood, ReadResponse::IronwoodSubtrees(subtrees)) => subtrees
                .values()
                .map(|subtree| {
                    Ok(SubtreeRoot {
                        root: TreeRoot::new(subtree.root.to_repr()),
                        end_height: subtree_end_height(subtree.end_height)?,
                    })
                })
                .collect(),
            _ => return Err(unexpected_response("Subtrees").into()),
        };

        Ok(roots?)
    }
}

impl zaino_source::OneShotGetAddressBalance for ZebraReadStateAdapter {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<
        zaino_primitives::types::AddressBalance,
        QueryError<zaino_source::GetAddressBalanceError, ReadStateError>,
    > {
        let valid = parse_addresses(addresses)?;

        match self.read(ReadRequest::AddressBalance(valid)).await? {
            ReadResponse::AddressBalance { balance, received } => {
                Ok(zaino_primitives::types::AddressBalance {
                    balance: zaino_primitives::types::Zatoshis::new(balance.into())
                        .map_err(ReadStateError::invalid_data)?,
                    // A lifetime receipts flow, delivered pre-summed by the
                    // state service; not supply-bounded, so it lands in the
                    // flow-sum type through its boundary door rather than
                    // being rejected by the amount bound.
                    received: zaino_primitives::types::ZatoshisFlowSum::from_summed(received),
                })
            }
            _ => Err(unexpected_response("AddressBalance").into()),
        }
    }
}

/// Validate transparent addresses before they reach the state service.
///
/// Rejection here is a domain error, not a transport one: the caller asked
/// about something that is not an address, and retrying will not change that.
fn parse_addresses<E>(
    addresses: Vec<String>,
) -> Result<
    std::collections::HashSet<zebra_chain::transparent::Address>,
    QueryError<E, ReadStateError>,
>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    addresses
        .into_iter()
        .map(|address| {
            address
                .parse::<zebra_chain::transparent::Address>()
                .map_err(|e| {
                    QueryError::NonDomain(ReadStateError::off_contract(format!(
                        "invalid transparent address `{address}`: {e}"
                    )))
                })
        })
        .collect()
}

/// Convert a zebra height, failing rather than substituting a placeholder.
fn subtree_end_height(height: zebra_chain::block::Height) -> Result<Height, ReadStateError> {
    Height::try_from(height.0).map_err(ReadStateError::invalid_data)
}

impl zaino_source::OneShotGetAddressUtxos for ZebraReadStateAdapter {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<
        Vec<zaino_primitives::types::Utxo>,
        QueryError<zaino_source::GetAddressUtxosError, ReadStateError>,
    > {
        use zaino_primitives::types::{Script, TransparentAddress, Utxo, Zatoshis};

        let valid = parse_addresses(addresses)?;

        let response = self.read(ReadRequest::UtxosByAddresses(valid)).await?;
        let utxos = match response {
            ReadResponse::AddressUtxos(utxos) => utxos,
            _ => return Err(unexpected_response("UtxosByAddresses").into()),
        };

        // Zebra documents this index as ordered by output location, and Zaino
        // relies on that rather than sorting. Verify it, but as a check that
        // fails the query — the previous implementation asserted, which turns a
        // corrupted index into a process abort.
        let mut previous =
            zebra_state::OutputLocation::from_usize(zebra_chain::block::Height(0), 0, 0);
        let mut result = Vec::new();

        for (address, txid, location, output) in utxos.utxos() {
            if result.is_empty() {
                // The first row has nothing to compare against; `previous` is a
                // floor, not a real predecessor.
                if location < &previous {
                    return Err(out_of_order("UTXO").into());
                }
            } else if location <= &previous {
                return Err(out_of_order("UTXO").into());
            }
            previous = *location;

            result.push(Utxo {
                address: TransparentAddress::new(address.to_string()),
                txid: zaino_primitives::types::TransactionId::from(txid.0),
                output_index: location.output_index().index(),
                script: Script::new(output.lock_script.as_raw_bytes().to_vec()),
                satoshis: Zatoshis::new(u64::from(output.value()))
                    .map_err(ReadStateError::invalid_data)?,
                height: Height::try_from(location.height().0)
                    .map_err(ReadStateError::invalid_data)?,
            });
        }

        Ok(result)
    }
}

impl zaino_source::OneShotGetAddressTxids for ZebraReadStateAdapter {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<
        Vec<zaino_primitives::types::TransactionId>,
        QueryError<zaino_source::GetAddressTxidsError, ReadStateError>,
    > {
        use zaino_primitives::types::TransactionId;

        // Bounds are checked against the tip before the index is queried, so an
        // impossible range is reported as such rather than silently returning
        // nothing.
        let tip = match self.read(ReadRequest::Tip).await? {
            ReadResponse::Tip(Some((height, _))) => height,
            ReadResponse::Tip(None) => {
                return Err(ReadStateError::off_contract("no blocks in chain").into())
            }
            _ => return Err(unexpected_response("Tip").into()),
        };

        if start > end {
            return Err(QueryError::Domain(
                zaino_source::GetAddressTxidsError::InvalidRange { start, end },
            ));
        }
        if u32::from(start) > tip.0 || u32::from(end) > tip.0 {
            return Err(QueryError::Domain(
                zaino_source::GetAddressTxidsError::InvalidRange { start, end },
            ));
        }

        let request = ReadRequest::TransactionIdsByAddresses {
            addresses: parse_addresses(addresses)?,
            height_range: zebra_chain::block::Height(u32::from(start))
                ..=zebra_chain::block::Height(u32::from(end)),
        };

        let hashes = match self.read(request).await? {
            ReadResponse::AddressesTransactionIds(hashes) => hashes,
            _ => return Err(unexpected_response("TransactionIdsByAddresses").into()),
        };

        // Chain order again checked rather than asserted.
        let mut previous =
            zebra_state::TransactionLocation::from_usize(zebra_chain::block::Height(0), 0);
        let mut result = Vec::new();

        for (location, txid) in hashes.iter() {
            if !result.is_empty() && location <= &previous {
                return Err(out_of_order("address transaction").into());
            }
            previous = *location;
            result.push(TransactionId::from(txid.0));
        }

        Ok(result)
    }
}

/// Deltas are synthesised from the address index rather than fetched.
///
/// Zebra has no `getaddressdeltas` RPC — the method is the legacy full node's — so on a Zebra
/// validator this is the only implementation there is. It rebuilds the answer
/// from two things the state service does have: the transparent address index,
/// which maps an address and height range to the transactions touching it, and
/// the transactions themselves.
///
/// # What this reports, and what it does not
///
/// Only *receives* (outputs paying a requested address) are reported. A spend
/// is an input naming a previous output, and the state service does not resolve
/// that outpoint back to the address and value it paid; recovering spends would
/// mean fetching every spent transaction as well. the legacy full node reports both, so a
/// caller comparing against it sees the receive half of each address's history.
/// This matches the behaviour of the connector this replaced, which built the
/// same answer through a verbose-transaction shape whose inputs likewise
/// carried no address.
impl zaino_source::OneShotGetAddressDeltas for ZebraReadStateAdapter {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<
        Vec<zaino_primitives::types::AddressDelta>,
        QueryError<zaino_source::GetAddressDeltasError, ReadStateError>,
    > {
        use zaino_primitives::types::{
            AddressDelta, SignedZatoshis, TransactionId, TransparentAddress,
        };

        if start > end {
            return Err(QueryError::Domain(
                zaino_source::GetAddressDeltasError::InvalidRange { start, end },
            ));
        }

        let request = ReadRequest::TransactionIdsByAddresses {
            addresses: parse_addresses(addresses.clone())?,
            height_range: zebra_chain::block::Height(u32::from(start))
                ..=zebra_chain::block::Height(u32::from(end)),
        };

        let located = match self.read(request).await? {
            ReadResponse::AddressesTransactionIds(located) => located,
            _ => return Err(unexpected_response("TransactionIdsByAddresses").into()),
        };

        // The index gives each transaction's location, so the height and the
        // position within the block come from it rather than from a second
        // lookup. Only the transaction body still has to be fetched.
        let mut deltas: Vec<AddressDelta> = Vec::new();

        for (location, txid) in located.iter() {
            let response = self
                .read(ReadRequest::AnyChainTransaction(txid.0.into()))
                .await?;

            let transaction = match response {
                ReadResponse::AnyChainTransaction(Some(zebra_state::AnyTx::Mined(mined))) => {
                    mined.tx
                }
                // The address index covers the best chain only, so a
                // transaction it named must be mined there. Anything else means
                // the index and the chain disagree.
                ReadResponse::AnyChainTransaction(_) => {
                    return Err(ReadStateError::off_contract(format!(
                        "address index names a transaction the chain lacks: {txid:?}"
                    ))
                    .into())
                }
                _ => return Err(unexpected_response("AnyChainTransaction").into()),
            };

            let height =
                Height::try_from(location.height.0).map_err(ReadStateError::invalid_data)?;
            let delta_txid = TransactionId::from(txid.0);

            for (index, output) in transaction.outputs().iter().enumerate() {
                let Some(address) = output.address(&self.network) else {
                    continue;
                };
                let address = address.to_string();
                if !addresses.iter().any(|requested| requested == &address) {
                    continue;
                }

                deltas.push(AddressDelta {
                    satoshis: SignedZatoshis::try_new(output.value.zatoshis())
                        .map_err(ReadStateError::invalid_data)?,
                    txid: delta_txid,
                    index: index as u32,
                    height,
                    address: TransparentAddress::new(address),
                    block_index: Some(u32::from(location.index.index())),
                });
            }
        }

        // the legacy full node orders deltas by (height, position in block, index within the
        // transaction). The address index carries each transaction's real
        // location, so this is the documented order rather than an
        // approximation of it.
        // `unwrap_or(MAX)` rather than letting `None` sort first: a delta whose
        // position in its block is unknown cannot be placed among those that
        // know theirs, so it goes last. Matches the ordering this replaced.
        deltas.sort_by_key(|delta| {
            (
                u32::from(delta.height),
                delta.block_index.unwrap_or(u32::MAX),
                delta.index,
            )
        });

        Ok(deltas)
    }
}

/// The ReadState adapter observes the chain directly, so it can offer a real
/// tip stream rather than a synthesised one.
impl zaino_source::SubscribeChainTip for ZebraReadStateAdapter {
    fn subscribe_to_chain_tip(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<zaino_source::TipObservation>> {
        // Not yet wired: `init_read_only` yields no `ChainTipChange`, so a tip
        // stream needs the syncer-backed construction path rather than the
        // read-only one. Until this adapter is built that way, a composite
        // should synthesise the capability with `PolledChainTip`.
        None
    }
}

impl zaino_source::SourceLifecycle for ZebraReadStateAdapter {
    /// Stop the syncer, if this adapter launched one.
    ///
    /// The database handle is released when the adapter drops, but a spawned
    /// syncer would outlive it and keep writing, so it is aborted explicitly.
    fn shutdown(&self) {
        if let Some(syncer) = &self.syncer {
            syncer.abort();
        }
    }
}

/// Reading the state directly gives no push notification of new blocks — that
/// signal belongs to the syncer, not the read handle.
impl zaino_source::SubscribeBlocks for ZebraReadStateAdapter {}

impl zaino_source::OneShotGetCommitmentTreeRoots for ZebraReadStateAdapter {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<
        zaino_primitives::types::TreeRoots,
        QueryError<zaino_source::GetCommitmentTreeRootsError, ReadStateError>,
    > {
        use zaino_primitives::types::TreeRoots;

        let id = hash_or_height(block);
        let zebra_hash = zebra_chain::block::Hash(block.into());

        // Read presence and the three pools concurrently: they are independent
        // reads and the caller waits for all of them regardless.
        let (depth, sapling, orchard, ironwood) = tokio::join!(
            self.read(ReadRequest::Depth(zebra_hash)),
            self.read(ReadRequest::SaplingTree(id)),
            self.read(ReadRequest::OrchardTree(id)),
            self.read(ReadRequest::IronwoodTree(id)),
        );

        // The finalized state holds only the finalized chain. A block above its
        // tip (the volatile top the chain-head serves) is absent here, and
        // reading its trees would return all-`None` — indistinguishable from a
        // genuine pre-activation block whose pools are legitimately empty. That
        // conflation is a lifecycle fact ("not in my finalized view") posing as a
        // domain answer ("no commitments"). Resolve it with a `Depth` presence
        // check and signal a domain miss, which the composite retries over
        // JSON-RPC (which sees the whole best chain), rather than serving a false
        // zero-size tree.
        let depth = match depth? {
            ReadResponse::Depth(depth) => depth,
            _ => return Err(unexpected_response("Depth").into()),
        };
        if depth.is_none() {
            return Err(QueryError::Domain(
                zaino_source::GetCommitmentTreeRootsError::BlockNotFound(block),
            ));
        }

        // Unlike the RPC path, the state service hands back a live tree, so the
        // root and count are read from it directly rather than deserialised.
        let sapling = match sapling? {
            ReadResponse::SaplingTree(tree) => tree
                .as_deref()
                .map(|tree| tree_root_info(tree.root().into(), tree.count()))
                .transpose()?,
            _ => return Err(unexpected_response("SaplingTree").into()),
        };
        let orchard = match orchard? {
            ReadResponse::OrchardTree(tree) => tree
                .as_deref()
                .map(|tree| tree_root_info(tree.root().into(), tree.count()))
                .transpose()?,
            _ => return Err(unexpected_response("OrchardTree").into()),
        };
        let ironwood = match ironwood? {
            ReadResponse::IronwoodTree(tree) => tree
                .as_deref()
                .map(|tree| tree_root_info(tree.root().into(), tree.count()))
                .transpose()?,
            _ => return Err(unexpected_response("IronwoodTree").into()),
        };

        Ok(TreeRoots {
            sapling,
            orchard,
            ironwood,
        })
    }
}

impl zaino_source::OneShotGetTreestateByHash for ZebraReadStateAdapter {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::Treestate,
        QueryError<zaino_source::GetTreestateByHashError, ReadStateError>,
    > {
        self.treestate(hash_or_height(hash))
            .await
            .map_err(QueryError::from)
    }
}

impl zaino_source::OneShotGetTreestate for ZebraReadStateAdapter {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<
        zaino_primitives::types::Treestate,
        QueryError<zaino_source::GetTreestateError, ReadStateError>,
    > {
        let id = zebra_chain::block::Height(u32::from(height)).into();
        self.treestate(id).await.map_err(QueryError::from)
    }
}

impl ZebraReadStateAdapter {
    /// Serialised commitment trees for a block, one per pool.
    ///
    /// Shared by the height- and hash-addressed queries, which differ only in
    /// how the block is named.
    ///
    /// # Activation gating removed deliberately
    ///
    /// The previous implementation looked up each pool's activation height and
    /// skipped the read for blocks below it. That made this a second source of
    /// activation-height truth, compiled in and able to disagree with the
    /// validator — the very thing Zaino avoids elsewhere by adopting the
    /// schedule from `getblockchaininfo`. The state service already answers
    /// `None` for a pool that is not active at the block, so the gate only
    /// duplicated a fact the read itself reports.
    async fn treestate(
        &self,
        id: zebra_state::HashOrHeight,
    ) -> Result<zaino_primitives::types::Treestate, ReadStateError> {
        // The header read identifies the block the trees belong to. A treestate
        // is only meaningful against one block, so it is fetched alongside
        // rather than left for the caller to pair up.
        let (header, sapling, orchard, ironwood) = tokio::join!(
            self.read(ReadRequest::BlockHeader(id)),
            self.read(ReadRequest::SaplingTree(id)),
            self.read(ReadRequest::OrchardTree(id)),
            self.read(ReadRequest::IronwoodTree(id)),
        );

        let (block_hash, height, time) = match header? {
            ReadResponse::BlockHeader {
                header,
                hash,
                height,
                ..
            } => (
                BlockHash::from(hash.0),
                Height::try_from(height.0).map_err(ReadStateError::invalid_data)?,
                header.time.timestamp() as u32,
            ),
            _ => return Err(unexpected_response("BlockHeader")),
        };

        // A pool with no tree at this block is absent rather than empty: the
        // block predates its activation. That distinction is why every pool is
        // an `Option` — see `Treestate`.
        // `final_root` is left absent here, matching the RPC adapter: roots are
        // answered by `get_commitment_tree_roots`, so a treestate carries the
        // tree and a caller that wants the root asks for it. Populating it on
        // one adapter only would make the answer depend on the transport.
        let pool = |final_state: Vec<u8>| zaino_primitives::types::PoolTreestate {
            final_root: None,
            final_state,
        };

        let sapling = match sapling? {
            ReadResponse::SaplingTree(tree) => {
                tree.as_deref().map(|tree| pool(tree.to_rpc_bytes()))
            }
            _ => return Err(unexpected_response("SaplingTree")),
        };
        let orchard = match orchard? {
            ReadResponse::OrchardTree(tree) => {
                tree.as_deref().map(|tree| pool(tree.to_rpc_bytes()))
            }
            _ => return Err(unexpected_response("OrchardTree")),
        };
        let ironwood = match ironwood? {
            ReadResponse::IronwoodTree(tree) => {
                tree.as_deref().map(|tree| pool(tree.to_rpc_bytes()))
            }
            _ => return Err(unexpected_response("IronwoodTree")),
        };

        Ok(zaino_primitives::types::Treestate {
            block_hash,
            height,
            time,
            sapling,
            orchard,
            ironwood,
        })
    }
}

/// The name a validator puts on the wire for a network upgrade.
///
/// Zebra's `Display` is its `Debug`, which spells NU5 as `Nu5` and NU6.1 as
/// `Nu6_1`. Neither is what a validator sends: the wire name comes from the
/// enum's serde renames, so it is read from there.
///
/// This has to agree with what the RPC adapter parses out of a validator's
/// reply, or the same chain gets two different upgrade names depending on which
/// transport answered — which `getlightdinfo` then reports to wallets.
fn upgrade_wire_name(upgrade: zebra_chain::parameters::NetworkUpgrade) -> String {
    match serde_json::to_value(upgrade) {
        Ok(serde_json::Value::String(name)) => name,
        // Unreachable for a unit variant, but a debug spelling is a better
        // answer than a panic in a field that is only ever displayed.
        _ => format!("{upgrade:?}"),
    }
}

/// Address a block by hash for the state service.
fn hash_or_height(hash: BlockHash) -> zebra_state::HashOrHeight {
    zebra_state::HashOrHeight::Hash(zebra_chain::block::Hash(hash.into()))
}

impl zaino_source::OneShotGetTransaction for ZebraReadStateAdapter {
    async fn get_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<
        zaino_source::TransactionResponse,
        QueryError<zaino_source::GetTransactionError, ReadStateError>,
    > {
        use zaino_primitives::types::TransactionLocation;
        use zebra_chain::serialization::ZcashSerialize;

        let zebra_txid = zebra_chain::transaction::Hash::from(<[u8; 32]>::from(txid));

        let response = self
            .read(ReadRequest::AnyChainTransaction(zebra_txid))
            .await?;

        let any_tx = match response {
            ReadResponse::AnyChainTransaction(tx) => tx,
            _ => return Err(unexpected_response("AnyChainTransaction").into()),
        };

        // `AnyChainTransaction` covers mined transactions on either chain. It
        // does not cover the mempool — the state service has no mempool — so an
        // absent transaction here means "not mined", not "does not exist". A
        // composite that also holds an RPC adapter should ask it before
        // concluding the transaction is unknown.
        let (transaction, location) = match any_tx {
            Some(zebra_state::AnyTx::Mined(mined)) => {
                let height =
                    Height::try_from(mined.height.0).map_err(ReadStateError::invalid_data)?;
                (mined.tx.clone(), TransactionLocation::BestChain(height))
            }
            Some(zebra_state::AnyTx::Side((transaction, _block_hash))) => {
                (transaction, TransactionLocation::NonBestChain)
            }
            None => {
                return Err(QueryError::Domain(
                    zaino_source::GetTransactionError::NotFound(txid),
                ))
            }
        };

        let bytes = transaction
            .zcash_serialize_to_vec()
            .map_err(ReadStateError::invalid_data)?;

        Ok(zaino_source::TransactionResponse { bytes, location })
    }
}

impl zaino_source::OneShotGetDifficulty for ZebraReadStateAdapter {
    async fn get_difficulty(
        &self,
    ) -> Result<
        zaino_primitives::types::Difficulty,
        QueryError<zaino_source::GetDifficultyError, ReadStateError>,
    > {
        // Reuses zebra's own calculation rather than reimplementing the
        // expansion from the compact threshold: difficulty is defined relative
        // to each network's minimum, and a second implementation of that would
        // be a second thing to keep correct.
        zebra_rpc::methods::chain_tip_difficulty(self.network.clone(), self.state.clone(), false)
            .await
            .map_err(|e| ReadStateError::unreachable(e).into())
    }
}

impl zaino_source::OneShotGetBlockchainInfo for ZebraReadStateAdapter {
    async fn get_blockchain_info(
        &self,
    ) -> Result<
        zaino_primitives::types::BlockchainInfo,
        QueryError<zaino_source::GetBlockchainInfoError, ReadStateError>,
    > {
        use zaino_primitives::types::{
            BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, NetworkUpgradeInfo,
            NetworkUpgradeStatus, ValuePoolBalance, Zatoshis,
        };
        use zebra_chain::parameters::NetworkUpgrade;

        let (height, hash, balance) = match self.read(ReadRequest::TipPoolValues).await? {
            ReadResponse::TipPoolValues {
                tip_height,
                tip_hash,
                value_balance,
            } => (tip_height, tip_hash, value_balance),
            _ => return Err(unexpected_response("TipPoolValues").into()),
        };

        let size_on_disk = match self.read(ReadRequest::UsageInfo).await? {
            ReadResponse::UsageInfo(size) => size,
            _ => return Err(unexpected_response("UsageInfo").into()),
        };

        let header = match self.read(ReadRequest::BlockHeader(hash.into())).await? {
            ReadResponse::BlockHeader { header, .. } => header,
            _ => return Err(unexpected_response("BlockHeader").into()),
        };

        // The estimate is clamped to the real tip in two cases: a tip whose
        // timestamp is in the future, and an estimate that lands below the tip
        // we already have. Either would otherwise report progress above 1.0 or
        // a network tip behind our own.
        let now = chrono::Utc::now();
        let estimate = zebra_chain::chain_tip::NetworkChainTipHeightEstimator::new(
            header.time,
            height,
            &self.network,
        )
        .estimate_height_at(now);
        let estimated_height = if header.time > now || estimate < height {
            height
        } else {
            estimate
        };

        // Zebra's activation list is the schedule this node is actually
        // enforcing, which is the point of reading it from the validator rather
        // than compiling one in. Upgrades without a consensus branch id are
        // zebra-internal rule changes with no the legacy full node equivalent, so they are
        // not part of the schedule a client can act on.
        let upgrades = self
            .network
            .full_activation_list()
            .into_iter()
            .filter_map(|(activation_height, upgrade)| {
                let branch_id = upgrade.branch_id()?;
                let status = if height >= activation_height {
                    NetworkUpgradeStatus::Active
                } else {
                    NetworkUpgradeStatus::Pending
                };
                Some(
                    Height::try_from(activation_height.0)
                        .map(|activation_height| NetworkUpgradeInfo {
                            branch_id: ConsensusBranchId::new(u32::from(branch_id)),
                            name: upgrade_wire_name(upgrade),
                            activation_height,
                            status,
                        })
                        .map_err(ReadStateError::invalid_data),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        let next_height = (height + 1)
            .ok_or_else(|| ReadStateError::off_contract("chain tip is at the maximum height"))?;
        let branch_at = |h| {
            NetworkUpgrade::current(&self.network, h)
                .branch_id()
                .map(|id| ConsensusBranchId::new(u32::from(id)))
                .unwrap_or(ConsensusBranchId::new(0))
        };

        let difficulty = zebra_rpc::methods::chain_tip_difficulty(
            self.network.clone(),
            self.state.clone(),
            false,
        )
        .await
        .map_err(ReadStateError::unreachable)?;

        let to_zatoshis =
            |amount: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>| {
                Zatoshis::new(amount.into()).map_err(ReadStateError::invalid_data)
            };

        Ok(BlockchainInfo {
            chain: self.network.bip70_network_name(),
            blocks: Height::try_from(height.0).map_err(ReadStateError::invalid_data)?,
            // The read-state serves the finalized chain, so validated headers
            // and processed blocks are the same height here.
            headers: Height::try_from(height.0).map_err(ReadStateError::invalid_data)?,
            estimated_height: Height::try_from(estimated_height.0)
                .map_err(ReadStateError::invalid_data)?,
            best_block_hash: BlockHash::from(hash.0),
            difficulty,
            verification_progress: f64::from(height.0) / f64::from(estimated_height.0),
            // Zebra does not store cumulative work per height
            // (ZcashFoundation/zebra#7109), so it is genuinely unknown here
            // rather than zero.
            chain_work: None,
            pruned: false,
            size_on_disk,
            // Not tracked by the read-state; the legacy full node counts sprout commitments
            // only, which has no meaning for a modern chain.
            commitments: 0,
            chain_supply: ValuePoolBalance {
                id: "transparent".to_string(),
                chain_value: to_zatoshis(balance.transparent_amount())?,
                monitored: true,
                value_delta: None,
            },
            value_pools: vec![
                ValuePoolBalance {
                    id: "sprout".to_string(),
                    chain_value: to_zatoshis(balance.sprout_amount())?,
                    monitored: true,
                    value_delta: None,
                },
                ValuePoolBalance {
                    id: "sapling".to_string(),
                    chain_value: to_zatoshis(balance.sapling_amount())?,
                    monitored: true,
                    value_delta: None,
                },
                ValuePoolBalance {
                    id: "orchard".to_string(),
                    chain_value: to_zatoshis(balance.orchard_amount())?,
                    monitored: true,
                    value_delta: None,
                },
            ],
            upgrades,
            consensus: ConsensusBranchIds {
                chain_tip: branch_at(height),
                next_block: branch_at(next_height),
            },
        })
    }
}

/// Serialize a block held by the state service back to its canonical bytes.
///
/// The state service hands back a parsed block, so the bytes are reproduced
/// rather than passed through. Zebra's serialization is the inverse of the
/// deserialization that produced the block, so the result is byte-identical to
/// what the hash commits to.
fn serialize_block(block: &zebra_chain::block::Block) -> Result<Vec<u8>, ReadStateError> {
    use zebra_chain::serialization::ZcashSerialize;
    block
        .zcash_serialize_to_vec()
        .map_err(ReadStateError::invalid_data)
}

impl zaino_source::OneShotGetRawBlock for ZebraReadStateAdapter {
    async fn get_raw_block(
        &self,
        height: Height,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockError, ReadStateError>> {
        let zebra_height = zebra_chain::block::Height(u32::from(height));

        match self.read(ReadRequest::Block(zebra_height.into())).await? {
            ReadResponse::Block(Some(block)) => Ok(serialize_block(&block)?),
            ReadResponse::Block(None) => Err(QueryError::Domain(
                zaino_source::GetBlockError::HeightNotFound(height),
            )),
            _ => Err(unexpected_response("Block").into()),
        }
    }
}

impl zaino_source::OneShotGetRawBlockByHash for ZebraReadStateAdapter {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockByHashError, ReadStateError>> {
        match self.read(ReadRequest::Block(hash_or_height(hash))).await? {
            ReadResponse::Block(Some(block)) => Ok(serialize_block(&block)?),
            // As with `GetBlockByHash`: absent here means "not in the finalized
            // state", so a composite retries over JSON-RPC before concluding
            // the block does not exist.
            ReadResponse::Block(None) => Err(QueryError::Domain(
                zaino_source::GetBlockByHashError::NotFound(hash),
            )),
            _ => Err(unexpected_response("Block").into()),
        }
    }
}

/// The block's difficulty as a multiple of the network's minimum.
///
/// The same calculation zebra serves on `getdifficulty` and on a verbose
/// block: divide the network's proof-of-work limit by the block's expanded
/// target, using the high 128 bits of each. Both are 256-bit values whose low
/// half is insignificant once converted to an `f64` mantissa, so the shift
/// costs nothing and keeps the division in range.
///
/// A target that does not expand is reported as `0.0`, matching zebra. That
/// branch is unreachable in practice — zebra's `CompactDifficulty` rejects an
/// unexpandable threshold at construction, so no such value can be built or
/// deserialized — but `to_expanded` is fallible in the type system and a
/// division by a zero target is not an option.
fn block_difficulty(
    threshold: zebra_chain::work::difficulty::CompactDifficulty,
    network: &Network,
) -> f64 {
    // Zebra's own `U256`, not `primitive_types`': the difficulty types convert
    // into this one, and mixing the two would need a byte round trip.
    use zebra_chain::work::difficulty::ParameterDifficulty as _;
    use zebra_chain::work::difficulty::U256;

    let Some(expanded) = threshold.to_expanded() else {
        return 0.0;
    };

    let limit: U256 = network.target_difficulty_limit().into();
    let limit = (limit >> 128).as_u128() as f64;
    let target = (U256::from(expanded) >> 128).as_u128() as f64;

    limit / target
}

/// The number of blocks `getblockdeltas` reports a median time over.
///
/// Consensus's median-time-past window: this block and the ten before it.
const MEDIAN_TIME_PAST_WINDOW: usize = 11;

impl ZebraReadStateAdapter {
    /// The median time past at `block`: the median timestamp of it and up to
    /// the ten blocks before it.
    ///
    /// Walks backwards by parent hash rather than by height so it follows the
    /// chain the block is actually on. A short walk is not an error — near
    /// genesis there simply are fewer than eleven blocks, and the median of
    /// what exists is the answer.
    async fn median_time_past(
        &self,
        block: &zebra_chain::block::Block,
    ) -> Result<u32, ReadStateError> {
        let mut times = Vec::with_capacity(MEDIAN_TIME_PAST_WINDOW);
        times.push(block.header.time.timestamp());

        let mut previous = block.header.previous_block_hash;
        for _ in 1..MEDIAN_TIME_PAST_WINDOW {
            // Genesis's parent hash is all zeroes and names no block, so the
            // read below simply misses and ends the walk.
            match self.read(ReadRequest::Block(previous.into())).await? {
                ReadResponse::Block(Some(parent)) => {
                    times.push(parent.header.time.timestamp());
                    previous = parent.header.previous_block_hash;
                }
                ReadResponse::Block(None) => break,
                _ => return Err(unexpected_response("Block")),
            }
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        u32::try_from(median).map_err(ReadStateError::invalid_data)
    }

    /// The transaction a spend refers to, for resolving the spent output's
    /// address and value.
    async fn prevout_transaction(
        &self,
        txid: zebra_chain::transaction::Hash,
    ) -> Result<Option<std::sync::Arc<zebra_chain::transaction::Transaction>>, ReadStateError> {
        match self.read(ReadRequest::AnyChainTransaction(txid)).await? {
            ReadResponse::AnyChainTransaction(Some(zebra_state::AnyTx::Mined(mined))) => {
                Ok(Some(mined.tx))
            }
            ReadResponse::AnyChainTransaction(_) => Ok(None),
            _ => Err(unexpected_response("AnyChainTransaction")),
        }
    }
}

impl zaino_source::OneShotGetBlockDeltas for ZebraReadStateAdapter {
    /// Derives `getblockdeltas` from the state service.
    ///
    /// # Why this is derived rather than proxied
    ///
    /// `getblockdeltas` is a legacy full-node method. **zebrad does not implement it** —
    /// it answers `-32601 Method not found` — so on a zebrad-backed deployment
    /// this derivation is the only implementation there is, not a second copy
    /// of one the validator already has.
    ///
    /// # What is attributed, and what is not
    ///
    /// Only inputs and outputs with exactly one derivable transparent address
    /// are reported, matching the legacy full node. A nonstandard script has no address to
    /// credit, and a bare multisig has no single owner; the legacy full node omits both
    /// rather than crediting the first address. So the deltas do not sum to a
    /// transaction's transparent balance and must not be used to derive one.
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockDeltas,
        QueryError<zaino_source::GetBlockDeltasError, ReadStateError>,
    > {
        use zaino_primitives::types::{
            rpc::{BlockDelta, BlockDeltas, InputDelta, OutputDelta},
            MerkleRoot, SignedZatoshis, TransactionId, TransparentAddress, Zatoshis,
        };
        use zebra_chain::serialization::ZcashSerialize as _;

        let zebra_hash = zebra_chain::block::Hash(hash.into());
        let block = match self.read(ReadRequest::Block(zebra_hash.into())).await? {
            ReadResponse::Block(Some(block)) => block,
            // As elsewhere: the read state holds the best chain only, so a miss
            // here is "not in the finalized state". The composite decides
            // whether to ask another transport before concluding it is absent.
            ReadResponse::Block(None) => {
                return Err(QueryError::Domain(
                    zaino_source::GetBlockDeltasError::BlockNotFound(hash),
                ))
            }
            _ => return Err(unexpected_response("Block").into()),
        };

        let height = block
            .coinbase_height()
            .ok_or_else(|| ReadStateError::off_contract("block has no coinbase height"))?;
        let domain_height = Height::try_from(height.0).map_err(ReadStateError::invalid_data)?;

        let tip = match self.read(ReadRequest::Tip).await? {
            ReadResponse::Tip(Some((tip_height, _))) => tip_height,
            ReadResponse::Tip(None) => {
                return Err(ReadStateError::off_contract("state service has no tip").into())
            }
            _ => return Err(unexpected_response("Tip").into()),
        };

        let next_block_hash = match self
            .read(ReadRequest::BestChainBlockHash(zebra_chain::block::Height(
                height.0 + 1,
            )))
            .await?
        {
            ReadResponse::BlockHash(next) => next.map(|next| BlockHash::from(next.0)),
            _ => return Err(unexpected_response("BlockHash").into()),
        };

        let mut deltas = Vec::with_capacity(block.transactions.len());
        for (tx_index, transaction) in block.transactions.iter().enumerate() {
            let mut inputs = Vec::new();
            for (index, input) in transaction.inputs().iter().enumerate() {
                // A coinbase input creates value rather than moving it, and
                // names no previous output to attribute.
                let Some(outpoint) = input.outpoint() else {
                    continue;
                };

                let previous = self
                    .prevout_transaction(outpoint.hash)
                    .await?
                    .ok_or_else(|| {
                        ReadStateError::off_contract(format!(
                            "getblockdeltas: prevout tx {} not in the chain",
                            outpoint.hash
                        ))
                    })?;
                let output = previous
                    .outputs()
                    .get(outpoint.index as usize)
                    .ok_or_else(|| {
                        ReadStateError::off_contract(format!(
                            "getblockdeltas: prevout index {} out of range for {}",
                            outpoint.index, outpoint.hash
                        ))
                    })?;

                let Some(address) = output.address(&self.network) else {
                    continue;
                };

                inputs.push(InputDelta {
                    address: TransparentAddress::new(address.to_string()),
                    // A spend debits the address, so the value leaves it.
                    satoshis: SignedZatoshis::try_new(-output.value.zatoshis())
                        .map_err(ReadStateError::invalid_data)?,
                    index: index as u32,
                    prev_txid: TransactionId::from(outpoint.hash.0),
                    prev_output: outpoint.index,
                });
            }

            let mut outputs = Vec::new();
            for (index, output) in transaction.outputs().iter().enumerate() {
                let Some(address) = output.address(&self.network) else {
                    continue;
                };
                outputs.push(OutputDelta {
                    address: TransparentAddress::new(address.to_string()),
                    satoshis: Zatoshis::new(u64::from(output.value))
                        .map_err(ReadStateError::invalid_data)?,
                    index: index as u32,
                });
            }

            deltas.push(BlockDelta {
                txid: TransactionId::from(transaction.hash().0),
                index: tx_index as u32,
                inputs,
                outputs,
            });
        }

        Ok(BlockDeltas {
            hash,
            confirmations: i64::from(tip.0.saturating_sub(height.0)) + 1,
            size: block
                .zcash_serialized_size()
                .try_into()
                .map_err(ReadStateError::invalid_data)?,
            height: domain_height,
            version: block.header.version,
            merkle_root: MerkleRoot::from(block.header.merkle_root.0),
            deltas,
            time: u32::try_from(block.header.time.timestamp())
                .map_err(ReadStateError::invalid_data)?,
            median_time: self.median_time_past(&block).await?,
            nonce: *block.header.nonce,
            // Same conversion `zaino-convert-zebra` uses for a block header:
            // zebra exposes no raw-bits accessor, so the nBits value crosses
            // as its display-order bytes, revalidated at the primitives door.
            bits: zaino_primitives::types::CompactDifficulty::try_from_be_bytes(
                block.header.difficulty_threshold.bytes_in_display_order(),
            )
            .map_err(ReadStateError::invalid_data)?,
            difficulty: block_difficulty(block.header.difficulty_threshold, &self.network),
            previous_block_hash: Some(BlockHash::from(block.header.previous_block_hash.0)),
            next_block_hash,
        })
    }
}

#[cfg(test)]
mod tree_root_info_tests {
    use super::tree_root_info;
    use zaino_primitives::types::TreeSize;
    use zaino_source::{FailureMode, NonDomainError};

    /// The largest count the compact protocol carries is accepted, and a full
    /// depth-32 tree (`2^32`) is refused rather than wrapped to zero (#549).
    #[test]
    fn a_count_past_u32_is_refused() {
        let max = tree_root_info([0; 32], u64::from(u32::MAX)).expect("u32::MAX fits");
        assert_eq!(max.size, TreeSize::from(u32::MAX));

        let full = tree_root_info([0; 32], 1_u64 << 32).expect_err("2^32 does not fit");
        // The adapter's `InvalidData` maps to the non-retryable `Parse` mode at the seam.
        assert_eq!(NonDomainError::from(full).mode, FailureMode::Parse);
    }
}

#[cfg(test)]
mod block_difficulty_tests {
    use super::block_difficulty;
    use zebra_chain::parameters::Network;
    use zebra_chain::work::difficulty::ParameterDifficulty as _;

    /// A block mined at exactly the network minimum is difficulty 1.0 — the
    /// unit the whole figure is expressed in, so getting it wrong rescales
    /// every other block's reported difficulty.
    #[test]
    fn the_network_minimum_is_difficulty_one() {
        let network = Network::new_regtest(Default::default());
        let minimum = network.target_difficulty_limit().to_compact();

        assert_eq!(block_difficulty(minimum, &network), 1.0);
    }

    /// Difficulty is inverse to the target: a harder block has a smaller
    /// target and a larger difficulty. An inverted division would still return
    /// plausible positive numbers, so the direction is pinned explicitly.
    #[test]
    fn a_harder_target_reports_a_higher_difficulty() {
        let network = Network::new_regtest(Default::default());
        let limit = network.target_difficulty_limit();
        let harder = (limit / 4).to_compact();

        let difficulty = block_difficulty(harder, &network);

        assert!(
            difficulty > 1.0,
            "a target below the network minimum must report difficulty above 1.0, got {difficulty}"
        );
    }
}
