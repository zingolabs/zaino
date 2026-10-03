//! `PassthroughProvider` — the passthrough provider.
//!
//! One half of the local/passthrough split: the validator answered **live**,
//! through the **canonical** `zaino-source` ports. It carries exactly the
//! capabilities the validator provides directly (broadcast today; treestate /
//! transaction / mempool / tip next), and never the ones zaino composes or
//! indexes itself — those belong to the local view (`ChainView`).
//!
//! It binds the canonical (resilient) traits, not the raw `OneShot*` ports: the
//! one-shots belong to the adapters that implement them and the
//! [`ValidatorClient`](zaino_source::ValidatorClient) decorator. A consumer binds
//! the twin, so resilience is already applied and this crate never touches a
//! single-attempt port. Which capabilities live here versus on the local view is
//! the classification — expressed as which provider carries the trait, and
//! checked where the two are composed (the engine and its coverage assertion).
//!
//! Reads answered here are **live, not pinned** to a snapshot's tip — inherent
//! to passthrough (the validator's state cannot be pinned to our view). That is
//! sound for the immutable, historical data light clients query.

use zaino_primitives::types::rpc::{BlockHeaderVerbose, MiningInfo, NodeInfo, PeerInfo};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockRef, BlockVerbose, BlockchainInfo,
    DecodedBlock, Height, HeightRange, PreIndexCompactTx, RawTransaction, ShieldedPool,
    SubtreeRoot, TransactionId, TransparentAddress, TransparentOutput, Treestate, Utxo,
};
use zaino_service::NodeStatusError;
use zaino_service::error::{
    AddressReadError, BlockReadError, BroadcastRejection, MempoolReadError, ReadError,
    TransactionViewError, TreestateReadError, TxReadError,
};
use zaino_service::{MempoolEntry, MempoolSummary};
use zaino_source::{
    DecodedTransaction, FailureMode, GetAddressBalance, GetAddressBalanceError, GetAddressDeltas,
    GetAddressDeltasError, GetAddressTxids, GetAddressTxidsError, GetAddressUtxos,
    GetAddressUtxosError, GetBlock, GetBlockByHash, GetBlockByHashError, GetBlockDecoded,
    GetBlockDecodedByHash, GetBlockError, GetBlockHeader, GetBlockHeaderError, GetBlockVerbose,
    GetBlockVerboseByHash, GetBlockVerboseError, GetBlockchainInfo, GetBlockchainInfoError,
    GetMempoolCompactTransaction, GetMempoolMetadata, GetMempoolMetadataError, GetMempoolSourceTip,
    GetMempoolTxids, GetMempoolTxidsError, GetMiningInfo, GetMiningInfoError, GetNetworkSolPs,
    GetNetworkSolPsError, GetNodeInfo, GetNodeInfoError, GetPeerInfo, GetPeerInfoError,
    GetRawBlock, GetRawBlockByHash, GetRawMempoolTransaction, GetRawMempoolTransactionError,
    GetSubtreeRoots, GetSubtreeRootsError, GetTransaction, GetTransactionError,
    GetTransactionVerbose, GetTransactionVerboseError, GetTreestate, GetTreestateError,
    SendRawTransaction, SendRawTransactionError, SourceError, TransactionResponse,
};

/// The passthrough provider over a resilient source handle `Src`.
pub struct PassthroughProvider<Src> {
    source: Src,
}

impl<Src: Clone> Clone for PassthroughProvider<Src> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
        }
    }
}

impl<Src> PassthroughProvider<Src> {
    /// Wrap a resilient source handle as the passthrough provider.
    pub fn new(source: Src) -> Self {
        Self { source }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: SendRawTransaction,
{
    /// Relay a wallet's transaction to the validator's send port — the only
    /// thing that can broadcast. Domain rejections map to the serving rejection;
    /// a transport failure surfaces as `Invalid` with the cause (a dedicated
    /// transient arm on the serving error surface is a follow-up).
    pub(crate) async fn broadcast(
        &self,
        raw_tx: Vec<u8>,
    ) -> Result<TransactionId, BroadcastRejection> {
        match self.source.send_raw_transaction(raw_tx).await {
            Ok(txid) => Ok(txid),
            Err(SourceError::Domain(SendRawTransactionError::Malformed(reason))) => {
                Err(BroadcastRejection::Malformed(reason))
            }
            Err(SourceError::Domain(SendRawTransactionError::Rejected(reason))) => {
                Err(BroadcastRejection::Invalid(reason))
            }
            Err(SourceError::NonDomain(cause)) => Err(BroadcastRejection::Invalid(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BroadcastRejection::Invalid(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlock,
{
    /// The full block at `height`, live from the validator. Passthrough: the
    /// finalised store holds compact projections, not full block bytes, so a
    /// full block can only come from the validator, which already returns the
    /// domain [`Block`]. A height with no block is a domain miss (`Ok(None)`); a
    /// transport failure is transient.
    pub(crate) async fn block(&self, height: Height) -> Result<Option<Block>, BlockReadError> {
        match self.source.get_block(height).await {
            Ok(block) => Ok(Some(block)),
            Err(SourceError::Domain(GetBlockError::HeightNotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockByHash,
{
    /// The full block with `hash`, live from the validator. See
    /// [`block`](Self::block) for why a full block is always passthrough. A hash
    /// no chain the validator retains holds is a domain miss (`Ok(None)`); a
    /// transport failure is transient.
    pub(crate) async fn block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Block>, BlockReadError> {
        match self.source.get_block_by_hash(hash).await {
            Ok(block) => Ok(Some(block)),
            Err(SourceError::Domain(GetBlockByHashError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetRawBlock,
{
    /// The raw consensus bytes of the block at `height`, live from the validator.
    /// Passthrough: the finalised store holds compact projections, not full block
    /// bytes, so the consensus-canonical form can only come from the validator. A
    /// height with no block is a domain miss (`Ok(None)`); a transport failure is
    /// transient.
    pub(crate) async fn raw_block(
        &self,
        height: Height,
    ) -> Result<Option<Vec<u8>>, BlockReadError> {
        match self.source.get_raw_block(height).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SourceError::Domain(GetBlockError::HeightNotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetRawBlockByHash,
{
    /// The raw consensus bytes of the block with `hash`, live from the validator.
    /// See [`raw_block`](Self::raw_block) for why raw block bytes are always
    /// passthrough; a hash can name a side-chain block. A hash no retained chain
    /// holds is a domain miss (`Ok(None)`); a transport failure is transient.
    pub(crate) async fn raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Vec<u8>>, BlockReadError> {
        match self.source.get_raw_block_by_hash(hash).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SourceError::Domain(GetBlockByHashError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockHeader,
{
    /// The verbose block header for `hash`, live from the validator. Passthrough:
    /// confirmations, difficulty, chainwork and the neighbouring hashes are
    /// cumulative chain state the validator derives, not facts in the stored
    /// block. A hash no chain the validator retains holds is a domain miss
    /// (`Ok(None)`); a transport failure is transient.
    pub(crate) async fn block_header_verbose(
        &self,
        hash: BlockHash,
    ) -> Result<Option<BlockHeaderVerbose>, BlockReadError> {
        match self.source.get_block_header(hash).await {
            Ok(header) => Ok(Some(header)),
            Err(SourceError::Domain(GetBlockHeaderError::BlockNotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockVerbose,
{
    /// The verbose chain-position facts for the block at `height`, live from the
    /// validator. See [`block_header_verbose`](Self::block_header_verbose) for
    /// why this is always passthrough. A height with no block is a domain miss
    /// (`Ok(None)`); a transport failure is transient.
    pub(crate) async fn block_verbose(
        &self,
        height: Height,
    ) -> Result<Option<BlockVerbose>, BlockReadError> {
        match self.source.get_block_verbose(height).await {
            Ok(block) => Ok(Some(block)),
            // The by-height port reports a miss as `HeightNotFound`; the shared
            // error's by-hash variant cannot arise here but is a miss all the
            // same, so both map to `Ok(None)` rather than a wildcard arm.
            Err(SourceError::Domain(GetBlockVerboseError::HeightNotFound(_))) => Ok(None),
            Err(SourceError::Domain(GetBlockVerboseError::BlockNotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockVerboseByHash,
{
    /// The verbose chain-position facts for the block with `hash`, live from the
    /// validator. See [`block_verbose`](Self::block_verbose); addressing by hash
    /// can name a side-chain block, where confirmations are negative and there is
    /// no next block. A hash no retained chain holds is a domain miss
    /// (`Ok(None)`); a transport failure is transient.
    pub(crate) async fn block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<BlockVerbose>, BlockReadError> {
        match self.source.get_block_verbose_by_hash(hash).await {
            Ok(block) => Ok(Some(block)),
            Err(SourceError::Domain(GetBlockVerboseError::HeightNotFound(_))) => Ok(None),
            Err(SourceError::Domain(GetBlockVerboseError::BlockNotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BlockReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

/// Map a source failure on an address read to the read surface. Each address
/// port supplies its own domain mapping (its rejections differ); the transport
/// arms are shared — an unreachable validator or an unusable answer is transient
/// on every one of them.
fn address_failure<E: std::fmt::Debug + std::fmt::Display>(
    err: SourceError<E>,
    domain: impl FnOnce(E) -> AddressReadError,
) -> AddressReadError {
    match err {
        SourceError::Domain(e) => domain(e),
        // A validator that does not implement the method answers with JSON-RPC
        // method-not-found. That is a gap in the validator, not a transient
        // transport failure, so it is its own typed case — the node-RPC adapter
        // reports it as method-not-found rather than an internal error. The
        // classification is on the typed failure mode, never the message.
        SourceError::NonDomain(cause) => {
            if is_method_not_found(&cause.mode) {
                AddressReadError::Unsupported(
                    "the validator does not implement this address method".to_owned(),
                )
            } else {
                AddressReadError::Transient(format!("validator unavailable: {cause}"))
            }
        }
        SourceError::Unavailable(cause) => {
            AddressReadError::Transient(format!("validator unavailable: {cause}"))
        }
    }
}

/// The JSON-RPC standard code for a method the server does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

/// Whether a source failure mode is the validator reporting method-not-found.
fn is_method_not_found(mode: &FailureMode) -> bool {
    matches!(mode, FailureMode::RpcError(code) if *code == METHOD_NOT_FOUND)
}

/// Convert the read surface's half-open `[start, end)` [`HeightRange`] to the
/// inclusive `[start, last]` bounds the address source ports take. Returns
/// `None` for an empty range, so the caller answers empty without troubling the
/// validator.
fn inclusive_bounds(range: HeightRange) -> Option<(Height, Height)> {
    let last = range.end.checked_sub(1)?;
    (range.start <= last).then_some((range.start, last))
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetAddressBalance,
{
    /// The transparent balance of `addr`, live from the validator.
    ///
    /// Stopgap: `getaddressbalance` is range-less, so the caller's requested
    /// [`HeightRange`] cannot be honoured — this answers the balance as of the
    /// validator's tip. A range-scoped balance is a reason to move address reads
    /// local. Passing transparent addresses to the validator also discloses them,
    /// the privacy cost a local transparent index exists to remove.
    pub(crate) async fn balance(
        &self,
        addr: &TransparentAddress,
    ) -> Result<AddressBalance, AddressReadError> {
        self.source
            .get_address_balance(vec![addr.as_str().to_owned()])
            .await
            .map_err(|err| {
                address_failure(err, |GetAddressBalanceError::InvalidAddress(a)| {
                    AddressReadError::Fatal(format!("invalid address: {a}"))
                })
            })
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetAddressUtxos,
{
    /// The unspent transparent outputs of `addr`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        self.source
            .get_address_utxos(vec![addr.as_str().to_owned()])
            .await
            .map_err(|err| {
                address_failure(err, |GetAddressUtxosError::InvalidAddress(a)| {
                    AddressReadError::Fatal(format!("invalid address: {a}"))
                })
            })
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetAddressTxids,
{
    /// The txids touching `addr` over `range`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        let Some((start, end)) = inclusive_bounds(range) else {
            return Ok(Vec::new());
        };
        self.source
            .get_address_txids(vec![addr.as_str().to_owned()], start, end)
            .await
            .map_err(|err| {
                address_failure(err, |domain| match domain {
                    GetAddressTxidsError::InvalidAddress(a) => {
                        AddressReadError::Fatal(format!("invalid address: {a}"))
                    }
                    GetAddressTxidsError::InvalidRange { start, end } => AddressReadError::Fatal(
                        format!("unserviceable height range {start}..={end}"),
                    ),
                })
            })
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetAddressDeltas,
{
    /// The balance deltas for `addr` over `range`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        let Some((start, end)) = inclusive_bounds(range) else {
            return Ok(Vec::new());
        };
        self.source
            .get_address_deltas(vec![addr.as_str().to_owned()], start, end)
            .await
            .map_err(|err| {
                address_failure(err, |domain| match domain {
                    GetAddressDeltasError::InvalidAddress(a) => {
                        AddressReadError::Fatal(format!("invalid address: {a}"))
                    }
                    GetAddressDeltasError::InvalidRange { start, end } => AddressReadError::Fatal(
                        format!("unserviceable height range {start}..={end}"),
                    ),
                })
            })
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetTreestate,
{
    /// The commitment treestate at `at`, live from the validator. Zaino does not
    /// index treestate, so this is passthrough. A height with no treestate is a
    /// definitive (non-retryable) answer; a transport failure is transient.
    pub(crate) async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        match self.source.get_treestate(at).await {
            Ok(treestate) => Ok(treestate),
            Err(SourceError::Domain(GetTreestateError::HeightNotFound(height))) => Err(
                TreestateReadError::Fatal(format!("no treestate at height {height}")),
            ),
            Err(SourceError::NonDomain(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetTransaction,
{
    /// The raw transaction bytes plus where it lives, live from the validator.
    /// Passthrough: zaino does not re-serialize — a wallet parses the bytes
    /// locally. A missing txid is a domain miss (`Ok(None)`); a transport failure
    /// is transient.
    pub(crate) async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        match self.source.get_transaction(id).await {
            Ok(TransactionResponse { bytes, location }) => Ok(Some(RawTransaction {
                data: bytes,
                location,
            })),
            Err(SourceError::Domain(GetTransactionError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetTransactionVerbose,
{
    /// The transaction decoded into its pool structure, plus where it lives
    /// ([`DecodedTransaction`]), live from the validator. Passthrough: the store
    /// holds no transaction bytes, and the pool decomposition needs the
    /// validator's chain library, which this crate must not depend on, so the
    /// decoding lives in the source adapter. A missing txid is a domain miss
    /// (`Ok(None)`); a transport failure is transient, with the cause formatted
    /// in.
    pub(crate) async fn transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<DecodedTransaction>, TxReadError> {
        match self.source.get_transaction_verbose(id).await {
            Ok(decoded) => Ok(Some(decoded)),
            Err(SourceError::Domain(GetTransactionVerboseError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockchainInfo,
{
    /// The validator's whole [`BlockchainInfo`], live. Passthrough as one piece:
    /// the aggregate describes a single chain position, so its fields come from
    /// one source rather than a mix that could place two heights in one answer.
    /// A validator still starting ([`GetBlockchainInfoError::NotReady`]) resolves
    /// on its own, so it is transient, not fatal — never a defaulted success,
    /// which would blank a consumer's view while the previous answer was still
    /// valid. A transport failure is transient too.
    pub(crate) async fn chain_info(&self) -> Result<BlockchainInfo, ReadError> {
        match self.source.get_blockchain_info().await {
            Ok(info) => Ok(info),
            Err(SourceError::Domain(GetBlockchainInfoError::NotReady)) => Err(
                ReadError::Transient("validator not ready to describe its chain".to_owned()),
            ),
            Err(SourceError::NonDomain(cause)) => Err(ReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(ReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetSubtreeRoots,
{
    /// Complete note-commitment subtree roots from `start_index`, live from the
    /// validator. Passthrough: the read is index-addressed exactly like the
    /// source. A pool that is not available is a definitive answer; a transport
    /// failure is transient.
    pub(crate) async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        match self
            .source
            .get_subtree_roots(pool, start_index, limit)
            .await
        {
            Ok(roots) => Ok(roots),
            Err(SourceError::Domain(GetSubtreeRootsError::PoolUnavailable(pool))) => Err(
                TreestateReadError::Fatal(format!("pool unavailable: {pool}")),
            ),
            Err(SourceError::NonDomain(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

// The three mempool reads bind separately (each to its own port) but share the
// single-source rule: a listing, its bytes, and its coherence tip must all come
// from the one source that serves the mempool — never a finalised secondary,
// which holds none. The routing that enforces that lives in the source adapter.
impl<Src> PassthroughProvider<Src>
where
    Src: GetMempoolTxids,
{
    /// The txids currently in the validator's mempool, live. A validator that
    /// exposes no mempool is served as an *empty* mempool (an honest answer, not
    /// a failure); a transport failure is transient.
    pub(crate) async fn mempool_txids(&self) -> Result<Vec<TransactionId>, MempoolReadError> {
        match self.source.get_mempool_txids().await {
            Ok(txids) => Ok(txids),
            Err(SourceError::Domain(GetMempoolTxidsError::Unavailable)) => Ok(Vec::new()),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetMempoolMetadata,
{
    /// The verbose mempool listing, live: each transaction with its size, fee,
    /// entry height and (optional) entry time. A validator that exposes no
    /// mempool is served as an *empty* mempool, matching
    /// [`mempool_txids`](Self::mempool_txids); a transport failure is transient.
    pub(crate) async fn mempool_entries(&self) -> Result<Vec<MempoolEntry>, MempoolReadError> {
        match self.source.get_mempool_metadata().await {
            Ok(entries) => Ok(entries
                .into_iter()
                .map(|meta| MempoolEntry {
                    txid: meta.txid,
                    size: meta.size,
                    fee: meta.fee,
                    entry_time: meta.entry_time,
                    entry_height: meta.entry_height,
                })
                .collect()),
            Err(SourceError::Domain(GetMempoolMetadataError::Unavailable)) => Ok(Vec::new()),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }

    /// Count and total serialized size of the validator's mempool, live, summed
    /// from the verbose listing. A validator that exposes no mempool is served as
    /// an *empty* mempool (count and bytes both zero); a transport failure is
    /// transient.
    pub(crate) async fn mempool_summary(&self) -> Result<MempoolSummary, MempoolReadError> {
        match self.source.get_mempool_metadata().await {
            Ok(entries) => {
                let size = u64::try_from(entries.len()).map_err(|_| {
                    MempoolReadError::Transient("mempool length overflows u64".into())
                })?;
                let bytes = entries
                    .iter()
                    .try_fold(0u64, |acc, meta| acc.checked_add(meta.size))
                    .ok_or_else(|| {
                        MempoolReadError::Transient("mempool byte total overflows u64".into())
                    })?;
                Ok(MempoolSummary { size, bytes })
            }
            Err(SourceError::Domain(GetMempoolMetadataError::Unavailable)) => {
                Ok(MempoolSummary::default())
            }
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetRawMempoolTransaction,
{
    /// The raw bytes of one mempool transaction, live. A txid the validator has
    /// since dropped — the listing/fetch race — is a domain miss (`Ok(None)`); a
    /// transport failure is transient.
    pub(crate) async fn raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<Vec<u8>>, MempoolReadError> {
        match self.source.get_raw_mempool_transaction(txid).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SourceError::Domain(GetRawMempoolTransactionError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetMempoolCompactTransaction,
{
    /// The compact projection of one mempool transaction, live. A txid the
    /// validator has since dropped is a domain miss (`Ok(None)`); a transport
    /// failure is transient. Shares [`GetRawMempoolTransactionError`] with the raw
    /// read it projects from.
    pub(crate) async fn mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<PreIndexCompactTx>, MempoolReadError> {
        match self.source.get_mempool_compact_transaction(txid).await {
            Ok(tx) => Ok(Some(tx)),
            Err(SourceError::Domain(GetRawMempoolTransactionError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetMempoolSourceTip,
{
    /// The chain tip the mempool listing is coherent against, live. The port
    /// carries no domain error (typed `Infallible`) — only a transport failure,
    /// reported transient.
    pub(crate) async fn mempool_source_tip(&self) -> Result<BlockRef, MempoolReadError> {
        match self.source.get_mempool_source_tip().await {
            Ok((hash, height)) => Ok(BlockRef { height, hash }),
            Err(SourceError::Domain(never)) => match never {},
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

/// Map a resilient source failure on a decoded read to the transaction-view
/// surface. The domain arm is the caller's to map — a miss on the requested
/// transaction or block is `Ok(None)` — while the two transport arms both
/// collapse to [`TransactionViewError::Unavailable`], keeping the cause as the
/// source chain. Factored out so those identical transport arms are written once
/// rather than per decoded read.
fn block_failure<E, T>(
    err: SourceError<E>,
    miss: impl FnOnce(E) -> Result<Option<T>, TransactionViewError>,
) -> Result<Option<T>, TransactionViewError>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    match err {
        SourceError::Domain(domain) => miss(domain),
        SourceError::NonDomain(cause) => Err(TransactionViewError::Unavailable {
            cause: Box::new(cause),
        }),
        SourceError::Unavailable(cause) => Err(TransactionViewError::Unavailable {
            cause: Box::new(cause),
        }),
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockDecoded,
{
    /// The whole block at `height` decoded into every transaction with its
    /// detail, live from the validator. Passthrough: the per-transaction decoding
    /// needs the validator's chain library, which this crate must not depend on. A
    /// height with no block is a domain miss (`Ok(None)`); a transport failure is
    /// [`TransactionViewError::Unavailable`].
    pub(crate) async fn block_decoded(
        &self,
        height: Height,
    ) -> Result<Option<DecodedBlock>, TransactionViewError> {
        match self.source.get_block_decoded(height).await {
            Ok(block) => Ok(Some(block)),
            Err(err) => block_failure(err, |GetBlockError::HeightNotFound(_)| Ok(None)),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetBlockDecodedByHash,
{
    /// The block with `hash` decoded into every transaction with its detail, live
    /// from the validator. Separate from [`block_decoded`](Self::block_decoded)
    /// because a hash can name a side-chain block. A hash no retained chain holds
    /// is a domain miss (`Ok(None)`); a transport failure is
    /// [`TransactionViewError::Unavailable`].
    pub(crate) async fn block_decoded_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<DecodedBlock>, TransactionViewError> {
        match self.source.get_block_decoded_by_hash(hash).await {
            Ok(block) => Ok(Some(block)),
            Err(err) => block_failure(err, |GetBlockByHashError::NotFound(_)| Ok(None)),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetTransactionVerbose,
{
    /// The transaction `id` decoded into its pool structure and detail, plus where
    /// it lives, live from the validator — the *requested* transaction of a
    /// transaction view. A missing txid is a domain miss (`Ok(None)`); a transport
    /// failure is [`TransactionViewError::Unavailable`].
    pub(crate) async fn transaction_decoded(
        &self,
        id: TransactionId,
    ) -> Result<Option<DecodedTransaction>, TransactionViewError> {
        match self.source.get_transaction_verbose(id).await {
            Ok(decoded) => Ok(Some(decoded)),
            Err(err) => block_failure(err, |GetTransactionVerboseError::NotFound(_)| Ok(None)),
        }
    }

    /// The transparent outputs of the transaction `id`, live from the validator —
    /// the spent transaction behind a prevout. `Ok(None)` when the validator does
    /// not know that txid, so the caller can name the specific outpoint as missing;
    /// a transport failure keeps its cause, for the caller to lift into
    /// [`TransactionViewError::Unavailable`]. The raw cause rather than the view
    /// error because a miss here is the caller's to interpret, which depends on the
    /// input that referenced it.
    pub(crate) async fn prevout_outputs(
        &self,
        id: TransactionId,
    ) -> Result<Option<Vec<TransparentOutput>>, Box<dyn std::error::Error + Send + Sync>> {
        match self.source.get_transaction_verbose(id).await {
            Ok(decoded) => Ok(Some(decoded.transaction.transparent.outputs)),
            Err(SourceError::Domain(GetTransactionVerboseError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(Box::new(cause)),
            Err(SourceError::Unavailable(cause)) => Err(Box::new(cause)),
        }
    }
}

// --- node-operator status: always passthrough, typed ---------------------
//
// Four facts about the validator, not the chain, so no index backs any of them.
// Each maps the source's typed ports onto `NodeStatusError`: a not-ready
// validator becomes ready, so it is its own arm; `NonDomain` and `Unavailable`
// both carry a typed cause, so both go through `NodeStatusError::unreachable`,
// which boxes it as a `#[source]` rather than formatting it into a message.

impl<Src> PassthroughProvider<Src>
where
    Src: GetNodeInfo,
{
    /// The validator's self-description, live.
    pub(crate) async fn node_info(&self) -> Result<NodeInfo, NodeStatusError> {
        match self.source.get_node_info().await {
            Ok(info) => Ok(info),
            Err(SourceError::Domain(GetNodeInfoError::NotReady)) => Err(NodeStatusError::NotReady),
            Err(SourceError::NonDomain(cause)) => Err(NodeStatusError::unreachable(cause)),
            Err(SourceError::Unavailable(cause)) => Err(NodeStatusError::unreachable(cause)),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetMiningInfo,
{
    /// The validator's mining view, live.
    pub(crate) async fn mining_info(&self) -> Result<MiningInfo, NodeStatusError> {
        match self.source.get_mining_info().await {
            Ok(info) => Ok(info),
            Err(SourceError::Domain(GetMiningInfoError::NotReady)) => {
                Err(NodeStatusError::NotReady)
            }
            Err(SourceError::NonDomain(cause)) => Err(NodeStatusError::unreachable(cause)),
            Err(SourceError::Unavailable(cause)) => Err(NodeStatusError::unreachable(cause)),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetPeerInfo,
{
    /// The validator's connected peers, live. An empty list is a valid answer
    /// from an isolated validator, never an error.
    pub(crate) async fn peer_info(&self) -> Result<Vec<PeerInfo>, NodeStatusError> {
        match self.source.get_peer_info().await {
            Ok(peers) => Ok(peers),
            Err(SourceError::Domain(GetPeerInfoError::NotReady)) => Err(NodeStatusError::NotReady),
            Err(SourceError::NonDomain(cause)) => Err(NodeStatusError::unreachable(cause)),
            Err(SourceError::Unavailable(cause)) => Err(NodeStatusError::unreachable(cause)),
        }
    }
}

impl<Src> PassthroughProvider<Src>
where
    Src: GetNetworkSolPs,
{
    /// The network solution rate, live, over `blocks` ending at `height` —
    /// forwarded as given, so `None` for either is the validator's own default.
    /// An unreachable validator errors rather than answering a zero value: a
    /// consumer that caches successes and ignores errors would otherwise cache a
    /// wrong number.
    pub(crate) async fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, NodeStatusError> {
        match self.source.get_network_sol_ps(blocks, height).await {
            Ok(rate) => Ok(rate),
            Err(SourceError::Domain(GetNetworkSolPsError::NotReady)) => {
                Err(NodeStatusError::NotReady)
            }
            Err(SourceError::NonDomain(cause)) => Err(NodeStatusError::unreachable(cause)),
            Err(SourceError::Unavailable(cause)) => Err(NodeStatusError::unreachable(cause)),
        }
    }
}
