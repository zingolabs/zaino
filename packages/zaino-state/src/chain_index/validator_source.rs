//! The new source stack, behind ChainIndex's scaffolding port.
//!
//! [`ZebraValidatorSource`] implements [`BlockchainSource`] — the temporary
//! port described in [`super::source`] — by delegating to
//! [`ZebraValidator`], the composite that routes each question to whichever
//! transport can answer it.
//!
//! # What this file is doing
//!
//! Translating *back*, for what is left of it. The port's signatures now carry
//! domain types almost throughout; the exceptions are `z_getblock` and
//! `getrawtransaction`, which are still assembled into `zebra-rpc` shapes here
//! because those presentation forms are built from a block's own bytes plus a
//! few chain facts, and the ports deliberately do not model them.
//!
//! That is deliberate and temporary. It is the price of leaving ChainIndex
//! untouched while the new stack is proven end to end underneath it, and every
//! conversion in this file is deleted with the scaffolding — as each ChainIndex
//! subsystem moves onto the real ports, its methods leave this file until
//! nothing is left.
//!
//! # Where a response is assembled rather than fetched
//!
//! Some wire shapes are *presentation*: the verbose block and the verbose
//! transaction list are derived from the block's own bytes plus a few
//! chain-state facts. The ports deliberately do not model them — that would
//! give one fact two sources — so this file assembles them, using zebra's own
//! builders (`TransactionObject::from_transaction`, `GetBlockTrees::new`) so
//! the formatting stays zebra's business rather than becoming Zaino's.

use std::sync::Arc;

use zaino_source::QueryError;
use zaino_source_zebra::ZebraValidator;
use zebra_rpc::methods::ValidateAddresses as _;
use zebra_state::HashOrHeight;

use super::source::{BlockchainSource, BlockchainSourceError, BlockchainSourceResult};
use super::source_ports::ChainIndexSourcePorts;

/// ChainIndex's validator source, backed by the `zaino-source` stack.
///
/// The single adapter between ChainIndex's driven port
/// ([`BlockchainSource`], still in wire types) and the driving ports
/// ([`zaino_source`], in domain types). It is generic over the source so that
/// conversion is written once: the production composite and the test mocks
/// reach ChainIndex through the same code, which makes the mock-backed suites
/// coverage of the conversion layer rather than of a parallel implementation
/// of it.
///
/// [`ZebraValidatorSource`] is the production instantiation.
pub struct ValidatorSource<V> {
    /// Shared because the port requires `Clone` and a source may own
    /// connections and a database handle that must not be duplicated.
    validator: Arc<V>,

    /// Needed to render verbose transactions: their presentation depends on the
    /// network's consensus branch schedule, which zebra's builder takes as an
    /// argument.
    network: zebra_chain::parameters::Network,

    /// Zebra's own tip-change stream, when this deployment reads the state
    /// database directly.
    ///
    /// Held rather than synthesised: the port hands out
    /// `zebra_state::ChainTipChange`, which cannot be built from the domain
    /// tip subscription. Keeping the real one preserves today's behaviour
    /// exactly, including that RPC-only deployments have none.
    chain_tip_change: Option<zebra_state::ChainTipChange>,
}

/// ChainIndex's source as deployed against a Zebra validator.
pub type ZebraValidatorSource = ValidatorSource<ZebraValidator>;

impl<V: ChainIndexSourcePorts> ValidatorSource<V> {
    /// Wrap anything answering ChainIndex's questions as its source.
    pub fn new(
        validator: V,
        network: zebra_chain::parameters::Network,
        chain_tip_change: Option<zebra_state::ChainTipChange>,
    ) -> Self {
        Self {
            validator: Arc::new(validator),
            network,
            chain_tip_change,
        }
    }

    /// The backing source.
    ///
    /// Tests drive a mock through a control surface that is deliberately not
    /// on any port — mining blocks is something a test fixture does, not a
    /// question a validator answers — so they need the concrete type back.
    pub fn source(&self) -> &V {
        &self.validator
    }
}

#[cfg(feature = "test_dependencies")]
impl ZebraValidatorSource {
    /// The backing state service, when this deployment reads the database.
    ///
    /// Test-only escape hatch, carried over from the deleted connector: live
    /// tests recompute expected chain data straight off the service.
    pub fn read_state_service(&self) -> Option<&zebra_state::ReadStateService> {
        self.validator
            .read_state()
            .map(|adapter| adapter.read_state_service())
    }
}

/// Written out rather than derived: `derive(Clone)` would demand `V: Clone`,
/// but the source is held behind an `Arc` precisely so it need not be — it owns
/// connections and a database handle that must not be duplicated.
impl<V> Clone for ValidatorSource<V> {
    fn clone(&self) -> Self {
        Self {
            validator: Arc::clone(&self.validator),
            network: self.network.clone(),
            chain_tip_change: self.chain_tip_change.clone(),
        }
    }
}

impl<V> std::fmt::Debug for ValidatorSource<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatorSource")
            .field("network", &self.network)
            .field("has_tip_stream", &self.chain_tip_change.is_some())
            .finish_non_exhaustive()
    }
}

/// Flatten a port error into the scaffolding port's single error type.
///
/// The ports distinguish a domain rejection from a transport fault; this error
/// cannot represent that, so the kind is kept in the message rather than lost.
/// The distinction becomes usable again as consumers move onto the port errors.
///
/// The transport fault is carried as the error's `source` rather than only
/// formatted into the message. `zaino-serve` recovers zcashd-compatible RPC
/// error codes by downcast-walking [`std::error::Error::source`] (see
/// `getblock_error_object_from_indexer_error` in
/// `zaino-serve/src/rpc/jsonrpc/service.rs`), so flattening a [`FetchError`] to
/// a string would strip the [`FailureMode::RpcError`] code those clients key
/// on.
///
/// A domain rejection is carried the same way, as a typed
/// [`LegacyRpcError`](crate::error::LegacyRpcError) with zcashd's
/// `InvalidParameter`. That is the code zcashd itself answers "not found" with,
/// and it is what reached clients before the adapters learned to tell a missing
/// object from an unreachable node — the reclassification must not cost the
/// served interface its error code.
///
/// [`FetchError`]: zaino_source::FetchError
/// [`FailureMode::RpcError`]: zaino_source::FailureMode::RpcError
fn err<E>(error: QueryError<E>) -> BlockchainSourceError
where
    E: std::fmt::Debug + std::fmt::Display,
{
    match error {
        QueryError::Domain(e) => BlockchainSourceError::unrecoverable_context(
            "validator rejected the query",
            crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                e.to_string(),
            ),
        ),
        QueryError::Fetch(e) => {
            BlockchainSourceError::unrecoverable_context("validator unreachable", e)
        }
    }
}

/// Flatten a `getspentinfo` port error, preserving zcashd's own error code.
///
/// The generic [`err`] reports every domain rejection as `InvalidParameter`
/// (`-8`), which is right where the interface has no more specific code to
/// offer. `getspentinfo` does: zcashd answers "no spend on record" with `-5`
/// (`InvalidAddressOrKey`) and the message `Unable to get spent info`, and that
/// is the pair a client matches on.
///
/// Neither Zaino nor its predecessor served `-5` here. The old path consumed the
/// code into a typed error, destroying the `RpcError` the serving layer walks
/// for, so a not-spent answer arrived as a generic internal error; the first
/// rewrite of this method then reported `-8`. Both are wrong in the same
/// direction — the code the client is looking for never reached it.
///
/// `Unsupported` has no zcashd code, because zcashd always implements the
/// method. It is reported as `-32601`, the envelope's own "method not found",
/// which is what the client would have seen had it asked the validator
/// directly — and is deliberately *not* `-5`: "this node cannot answer" must
/// not be read as "the output is unspent".
fn spent_info_err(error: QueryError<zaino_source::GetSpentInfoError>) -> BlockchainSourceError {
    use zaino_source::GetSpentInfoError;

    let (code, message) = match error {
        QueryError::Domain(rejection @ GetSpentInfoError::NotSpent) => (
            zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64,
            rejection.to_string(),
        ),
        QueryError::Domain(rejection @ GetSpentInfoError::Unsupported) => {
            (METHOD_NOT_FOUND, rejection.to_string())
        }
        // A transport fault is not an answer; `err` already carries it with the
        // typed cause the serving layer needs.
        fetch @ QueryError::Fetch(_) => return err(fetch),
    };

    BlockchainSourceError::unrecoverable_context(
        "validator rejected the query",
        crate::error::LegacyRpcError { code, message },
    )
}

/// The JSON-RPC envelope's "method not found".
///
/// Not a zcashd legacy code: zcashd implements every method Zaino forwards, so
/// this only arises when the backing validator is not zcashd.
const METHOD_NOT_FOUND: i64 = -32601;

/// A domain height from a zebra one, rejecting values the protocol disallows.
fn height(
    h: zebra_chain::block::Height,
) -> Result<zaino_primitives::types::Height, BlockchainSourceError> {
    zaino_primitives::types::Height::try_from(h.0)
        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))
}

/// A domain block hash from a zebra one. Same internal byte order on both
/// sides; the display-order reversal happens in the adapters, below this layer.
fn hash(h: zebra_chain::block::Hash) -> zaino_primitives::types::BlockHash {
    zaino_primitives::types::BlockHash::from(h.0)
}

/// Deserialize canonical block bytes back into a zebra block.
///
/// The port serves blocks as protocol bytes precisely so that consumers which
/// build their own representation — as ChainIndex does — get exactly what the
/// block hash commits to. This is where that round trip closes.
fn block_from_bytes(
    bytes: Vec<u8>,
) -> Result<Arc<zebra_chain::block::Block>, BlockchainSourceError> {
    use zebra_chain::serialization::ZcashDeserialize as _;
    zebra_chain::block::Block::zcash_deserialize(bytes.as_slice())
        .map(Arc::new)
        .map_err(|e| {
            BlockchainSourceError::Unrecoverable(format!("block did not deserialize: {e}"))
        })
}

/// Reject a request whose addresses are not valid transparent addresses.
///
/// Validated here rather than passed through, so a malformed request fails
/// before it reaches the validator, as it does today.
fn address_strings_to_vec(
    request: &zebra_rpc::client::GetAddressBalanceRequest,
) -> Result<Vec<String>, BlockchainSourceError> {
    Ok(request
        .valid_addresses()
        .map_err(|e| invalid(format!("invalid address: {e}")))?
        .into_iter()
        .map(|address| address.to_string())
        .collect())
}

/// A domain height from a bare `u32` request field.
fn domain_height(h: u32) -> Result<zaino_primitives::types::Height, BlockchainSourceError> {
    zaino_primitives::types::Height::try_from(h)
        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))
}

fn invalid(message: String) -> BlockchainSourceError {
    BlockchainSourceError::Unrecoverable(message)
}

impl<V: ChainIndexSourcePorts> ValidatorSource<V> {
    /// Identify the block at a height, for the chaininfo delta form.
    async fn block_info(
        &self,
        height: zaino_primitives::types::Height,
    ) -> Result<zaino_primitives::types::BlockRef, BlockchainSourceError> {
        let bytes = self.validator.get_raw_block(height).await.map_err(err)?;
        let block = block_from_bytes(bytes)?;
        Ok(zaino_primitives::types::BlockRef {
            hash: zaino_primitives::types::BlockHash::from(block.hash().0),
            height,
        })
    }
}

/// Parse a txid written in RPC display order.
fn parse_display_txid(
    hex_str: &str,
) -> Result<zaino_primitives::types::TransactionId, BlockchainSourceError> {
    let bytes = hex::decode(hex_str).map_err(|e| invalid(format!("txid is not hex: {e}")))?;
    let mut internal: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("txid is not 32 bytes".to_string()))?;
    internal.reverse();
    Ok(zaino_primitives::types::TransactionId::from(internal))
}

/// This crate's shielded pool as the port names it.
///
/// The two enums share a name and their variants, but not a role: this crate's
/// also carries activation semantics that a zero-dependency crate cannot hold.
fn domain_pool(pool: crate::chain_index::ShieldedPool) -> zaino_primitives::types::ShieldedPool {
    match pool {
        crate::chain_index::ShieldedPool::Sapling => zaino_primitives::types::ShieldedPool::Sapling,
        crate::chain_index::ShieldedPool::Orchard => zaino_primitives::types::ShieldedPool::Orchard,
        crate::chain_index::ShieldedPool::Ironwood => {
            zaino_primitives::types::ShieldedPool::Ironwood
        }
    }
}

/// A sapling tree root as zebra's own type, paired with its size.
///
/// Fallible: 32 bytes that are not a point on the pool's curve cannot be a
/// commitment tree root, so the validator sent something impossible rather than
/// something absent.
fn sapling_root(
    info: zaino_primitives::types::TreeRootInfo,
) -> Result<(zebra_chain::sapling::tree::Root, u64), BlockchainSourceError> {
    let bytes: [u8; 32] = info.root.into();
    let root = zebra_chain::sapling::tree::Root::try_from(bytes)
        .map_err(|_| invalid("sapling tree root is not a valid curve point".to_string()))?;
    Ok((root, info.size))
}

/// Orchard and Ironwood share a root type, as they share an action shape.
fn orchard_root(
    info: zaino_primitives::types::TreeRootInfo,
) -> Result<(zebra_chain::orchard::tree::Root, u64), BlockchainSourceError> {
    let bytes: [u8; 32] = info.root.into();
    let root = zebra_chain::orchard::tree::Root::try_from(bytes)
        .map_err(|_| invalid("orchard tree root is not a valid curve point".to_string()))?;
    Ok((root, info.size))
}

/// Parse a 32-byte identifier written in RPC display order.
fn parse_display_hash32(hex_str: &str) -> Result<[u8; 32], BlockchainSourceError> {
    let bytes = hex::decode(hex_str).map_err(|e| invalid(format!("not hex: {e}")))?;
    let mut internal: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("identifier is not 32 bytes".to_string()))?;
    internal.reverse();
    Ok(internal)
}

/// A signed zatoshi amount as zebra's checked type.
fn amount<C: zebra_chain::amount::Constraint>(
    zats: i64,
) -> Result<zebra_chain::amount::Amount<C>, BlockchainSourceError> {
    zebra_chain::amount::Amount::try_from(zats)
        .map_err(|e| invalid(format!("amount out of range: {e}")))
}

/// One value pool as the interface's balance type, keyed by pool name.
///
/// The name is how the interface identifies a pool, so an unrecognised one is
/// rejected rather than silently filed under the wrong pool.
fn pool_balance(
    balance: Option<&zaino_primitives::types::ValuePoolBalance>,
) -> Result<zebra_rpc::client::GetBlockchainInfoBalance, BlockchainSourceError> {
    use zebra_rpc::client::GetBlockchainInfoBalance;

    let Some(balance) = balance else {
        return Ok(GetBlockchainInfoBalance::chain_supply(Default::default()));
    };
    let value = amount(
        i64::try_from(u64::from(balance.chain_value))
            .map_err(|_| invalid("pool balance out of range".to_string()))?,
    )?;
    let delta = balance
        .value_delta
        .map(|d| amount(i64::from(d)))
        .transpose()?;

    Ok(match balance.id.as_str() {
        "transparent" => GetBlockchainInfoBalance::transparent(value, delta),
        "sprout" => GetBlockchainInfoBalance::sprout(value, delta),
        "sapling" => GetBlockchainInfoBalance::sapling(value, delta),
        "orchard" => GetBlockchainInfoBalance::orchard(value, delta),
        // zebra names this pool `lockbox` on the wire; `deferred` is zcashd's
        // name for the same pool, and zebra's own constructor is still called
        // `deferred`. Both spellings are accepted so the answer does not depend
        // on which validator is behind the adapter.
        "lockbox" | "deferred" => GetBlockchainInfoBalance::deferred(value, delta),
        "ironwood" => GetBlockchainInfoBalance::ironwood(value, delta),
        // `chainSupply` is a total rather than a pool, and arrives unnamed.
        "" => GetBlockchainInfoBalance::chain_supply(Default::default()),
        other => return Err(invalid(format!("unknown value pool `{other}`"))),
    })
}

/// The interface reports value pools as a fixed six-slot array, in a defined
/// order. Pools the validator did not report are zero rather than absent —
/// there is no slot for "unknown".
fn value_pool_array(
    pools: &[zaino_primitives::types::ValuePoolBalance],
) -> Result<zebra_rpc::methods::BlockchainValuePoolBalances, BlockchainSourceError> {
    let mut slots = zebra_rpc::client::GetBlockchainInfoBalance::zero_pools();
    for pool in pools {
        let built = pool_balance(Some(pool))?;
        let slot = match pool.id.as_str() {
            "transparent" => 0,
            "sprout" => 1,
            "sapling" => 2,
            "orchard" => 3,
            "lockbox" | "deferred" => 4,
            "ironwood" => 5,
            other => return Err(invalid(format!("unknown value pool `{other}`"))),
        };
        slots[slot] = built;
    }
    Ok(slots)
}

// ---------------------------------------------------------------------------
// Mempool ports, forwarded
//
// The mempool subsystem reads the validator through `zaino-source` directly
// rather than through `BlockchainSource`, so these four pass straight through
// with no conversion — there is nothing to convert, both sides already speak
// domain types. They exist because ChainIndex hands its source to the mempool,
// and a trait impl does not travel through a wrapper on its own.
//
// All four must reach the *same* transport, which they do: `ZebraValidator`
// routes them together, and the mempool's coherence check depends on it.
// ---------------------------------------------------------------------------

impl<V: ChainIndexSourcePorts> zaino_source::GetMempoolTxids for ValidatorSource<V> {
    async fn get_mempool_txids(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::TransactionId>,
        zaino_source::QueryError<zaino_source::GetMempoolTxidsError>,
    > {
        self.validator.get_mempool_txids().await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::GetMempoolMetadata for ValidatorSource<V> {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<
        Vec<zaino_source::MempoolTxMeta>,
        zaino_source::QueryError<zaino_source::GetMempoolMetadataError>,
    > {
        self.validator.get_mempool_metadata().await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::GetRawMempoolTransaction for ValidatorSource<V> {
    async fn get_raw_mempool_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<Vec<u8>, zaino_source::QueryError<zaino_source::GetRawMempoolTransactionError>>
    {
        self.validator.get_raw_mempool_transaction(txid).await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::GetMempoolSourceTip for ValidatorSource<V> {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<
        (
            zaino_primitives::types::BlockHash,
            zaino_primitives::types::Height,
        ),
        zaino_source::QueryError<std::convert::Infallible>,
    > {
        self.validator.get_mempool_source_tip().await
    }
}

impl<V: ChainIndexSourcePorts> BlockchainSource for ValidatorSource<V> {
    // ***** Blocks *****

    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        // The port splits by-height from by-hash because they are different
        // questions — a height names a best-chain block, a hash can name a
        // side-chain one — so the scaffolding's combined argument is resolved
        // here rather than pushed down. The two arms carry different error
        // types, which is why each is matched where it is produced.
        //
        // A missing block is `None` here, not an error: that is what the
        // scaffolding's callers expect.
        let bytes = match id {
            HashOrHeight::Height(h) => match self.validator.get_raw_block(height(h)?).await {
                Ok(bytes) => bytes,
                Err(QueryError::Domain(_)) => return Ok(None),
                Err(e) => return Err(err(e)),
            },
            HashOrHeight::Hash(h) => match self.validator.get_raw_block_by_hash(hash(h)).await {
                Ok(bytes) => bytes,
                Err(QueryError::Domain(_)) => return Ok(None),
                Err(e) => return Err(err(e)),
            },
        };

        Ok(Some(block_from_bytes(bytes)?))
    }

    // ***** Chain *****

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        let (hash, _height) = self.validator.get_chain_tip().await.map_err(err)?;
        Ok(Some(zebra_chain::block::Hash(hash.into())))
    }

    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        let height = self.validator.get_best_block_height().await.map_err(err)?;
        Ok(Some(zebra_chain::block::Height(height.into())))
    }

    async fn get_difficulty(&self) -> BlockchainSourceResult<f64> {
        self.validator.get_difficulty().await.map_err(err)
    }

    fn chain_tip_change(&self) -> Option<zebra_state::ChainTipChange> {
        self.chain_tip_change.clone()
    }

    // ***** Transparent addresses *****

    async fn get_address_balance(
        &self,
        address_strings: zebra_rpc::client::GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<zaino_primitives::types::AddressBalance> {
        self.validator
            .get_address_balance(address_strings_to_vec(&address_strings)?)
            .await
            .map_err(err)
    }

    async fn get_address_txids(
        &self,
        request: zebra_rpc::client::GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<super::types::TransactionHash>> {
        let (addresses, start, end) = request.into_parts();

        let txids = self
            .validator
            .get_address_txids(addresses, domain_height(start)?, domain_height(end)?)
            .await
            .map_err(err)?;

        Ok(txids
            .into_iter()
            .map(|txid| super::types::TransactionHash::from(<[u8; 32]>::from(txid)))
            .collect())
    }

    async fn get_address_utxos(
        &self,
        address_strings: zebra_rpc::client::GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<zaino_primitives::types::Utxo>> {
        self.validator
            .get_address_utxos(address_strings_to_vec(&address_strings)?)
            .await
            .map_err(err)
    }

    async fn get_address_deltas(
        &self,
        params: zaino_primitives::types::rpc::AddressDeltasRequest,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::AddressDeltas> {
        use zaino_primitives::types::rpc::{AddressDeltas, AddressDeltasRequest};

        let (addresses, start, end, chain_info) = match params {
            AddressDeltasRequest::Filtered {
                addresses,
                start,
                end,
                chain_info,
            } => (addresses, start, end, chain_info),
            // The single-address form carries no range, which the interface
            // reads as "the whole chain".
            AddressDeltasRequest::Address(address) => (vec![address], 0, 0, false),
        };
        let addresses: Vec<String> = addresses.into_iter().map(String::from).collect();

        // The interface's range is open-ended and unvalidated; the port's is
        // neither, so the bounds are resolved against the tip here.
        let tip = zaino_source::GetBestBlockHeight::get_best_block_height(&*self.validator)
            .await
            .map_err(err)?;
        let (start, end) = clamp_deltas_range_to_tip(tip, start, end);

        let deltas = self
            .validator
            .get_address_deltas(addresses, start, end)
            .await
            .map_err(err)?;

        if !chain_info {
            return Ok(AddressDeltas::Simple(deltas));
        }

        // The chaininfo form additionally names the range's bounding blocks.
        //
        // Both bounds name a real block by this point, genesis included. The
        // connector this replaced also required `start > 0 && end > 0`, but
        // that was standing in for "no range was given" — the single-address
        // form arrived here as `0..=0` before anything clamped it. The clamp
        // above resolves that case, so a zero bound now means genesis and
        // nothing else, and genesis is as nameable as any other block.
        Ok(AddressDeltas::WithChainInfo {
            deltas,
            start: self.block_info(start).await?,
            end: self.block_info(end).await?,
        })
    }

    // ***** Transactions and verbose blocks *****
    //
    // The two methods that assemble a response rather than translate one. The
    // ports do not model these presentation shapes — they are derived from the
    // block's own bytes plus a few chain facts, and modelling them would give
    // one fact two sources — so they are built here using zebra's own builders.

    async fn get_transaction(
        &self,
        txid: super::types::TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            super::source::GetTransactionLocation,
        )>,
    > {
        use super::source::GetTransactionLocation;
        use zaino_primitives::types::TransactionLocation;
        use zebra_chain::serialization::ZcashDeserialize as _;

        let domain_txid = zaino_primitives::types::TransactionId::from(txid.0);

        let response = match self.validator.get_transaction(domain_txid).await {
            Ok(response) => response,
            // The scaffolding reports an unknown transaction as `None`, not an
            // error. The composite has already asked both transports, so a
            // domain miss here means genuinely absent.
            Err(QueryError::Domain(_)) => return Ok(None),
            Err(e) => return Err(err(e)),
        };

        let transaction =
            zebra_chain::transaction::Transaction::zcash_deserialize(response.bytes.as_slice())
                .map_err(|e| invalid(format!("transaction did not deserialize: {e}")))?;

        let location = match response.location {
            TransactionLocation::BestChain(height) => {
                GetTransactionLocation::BestChain(zebra_chain::block::Height(height.into()))
            }
            TransactionLocation::NonBestChain => GetTransactionLocation::NonbestChain,
            TransactionLocation::Mempool => GetTransactionLocation::Mempool,
        };

        Ok(Some((Arc::new(transaction), location)))
    }

    async fn get_block_verbose(
        &self,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> BlockchainSourceResult<zebra_rpc::methods::GetBlock> {
        use zebra_rpc::methods::{GetBlock, GetBlockTransaction, GetBlockTrees};

        let verbosity = verbosity.unwrap_or(1);

        // Verbosity 0 is the serialized block and nothing else, which is what
        // the raw port already serves.
        let raw = match hash_or_height {
            HashOrHeight::Height(h) => self
                .validator
                .get_raw_block(height(h)?)
                .await
                .map_err(err)?,
            HashOrHeight::Hash(h) => self
                .validator
                .get_raw_block_by_hash(hash(h))
                .await
                .map_err(err)?,
        };

        if verbosity == 0 {
            return Ok(GetBlock::Raw(raw.into()));
        }

        let block = block_from_bytes(raw.clone())?;
        let block_hash = block.hash();
        let block_height = block
            .coinbase_height()
            .ok_or_else(|| invalid("block has no coinbase height".to_string()))?;

        // The chain-state facts the block itself cannot supply.
        let (verbose, roots) = tokio::try_join!(
            async {
                self.validator
                    .get_block_verbose_by_hash(hash(block_hash))
                    .await
                    .map_err(err)
            },
            async {
                self.validator
                    .get_commitment_tree_roots(hash(block_hash))
                    .await
                    .map_err(err)
            },
        )?;

        let block_time = block.header.time;

        // Verbosity 2 carries whole transaction objects, 1 carries their ids.
        // Both are built by zebra, so their formatting stays zebra's business
        // rather than becoming Zaino's to keep in step.
        let tx = if verbosity >= 2 {
            block
                .transactions
                .iter()
                .map(|transaction| {
                    GetBlockTransaction::Object(Box::new(
                        zebra_rpc::client::TransactionObject::from_transaction(
                            transaction.clone(),
                            Some(block_height),
                            Some(verbose.confirmations),
                            &self.network,
                            Some(block_time),
                            Some(block_hash),
                            Some(verbose.confirmations >= 0),
                            transaction.hash(),
                        ),
                    ))
                })
                .collect()
        } else {
            block
                .transactions
                .iter()
                .map(|transaction| GetBlockTransaction::Hash(transaction.hash()))
                .collect()
        };

        Ok(GetBlock::Object(Box::new(
            zebra_rpc::methods::BlockObject::new(
                block_hash,
                verbose.confirmations,
                Some(raw.len() as i64),
                Some(block_height),
                Some(block.header.version),
                Some(block.header.merkle_root),
                Some(*block.header.commitment_bytes),
                roots.sapling.map(|info| <[u8; 32]>::from(info.root)),
                roots.orchard.map(|info| <[u8; 32]>::from(info.root)),
                block.transactions.len(),
                tx,
                Some(block_time.timestamp()),
                Some(*block.header.nonce),
                Some(block.header.solution),
                Some(block.header.difficulty_threshold),
                Some(verbose.difficulty),
                match verbose.chain_supply.as_ref() {
                    Some(supply) => Some(pool_balance(Some(supply))?),
                    None => None,
                },
                if verbose.value_pools.is_empty() {
                    None
                } else {
                    Some(value_pool_array(&verbose.value_pools)?)
                },
                GetBlockTrees::new(
                    verbose.tree_sizes.sapling,
                    verbose.tree_sizes.orchard,
                    verbose.tree_sizes.ironwood,
                ),
                Some(block.header.previous_block_hash),
                verbose
                    .next_block_hash
                    .map(|h| zebra_chain::block::Hash(h.into())),
            ),
        )))
    }

    // ***** Headers, deltas, chain info *****

    async fn get_block_header(
        &self,
        hash: String,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::BlockHeaderVerbose> {
        self.validator
            .get_block_header(zaino_primitives::types::BlockHash::from(
                parse_display_hash32(&hash)?,
            ))
            .await
            .map_err(err)
    }

    async fn get_raw_block_header(&self, hash: String) -> BlockchainSourceResult<Vec<u8>> {
        self.validator
            .get_raw_block_header(zaino_primitives::types::BlockHash::from(
                parse_display_hash32(&hash)?,
            ))
            .await
            .map_err(err)
    }

    async fn get_block_deltas(
        &self,
        hash: String,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::BlockDeltas> {
        self.validator
            .get_block_deltas(zaino_primitives::types::BlockHash::from(
                parse_display_hash32(&hash)?,
            ))
            .await
            .map_err(err)
    }

    async fn get_blockchain_info(
        &self,
    ) -> BlockchainSourceResult<zaino_primitives::types::BlockchainInfo> {
        self.validator.get_blockchain_info().await.map_err(err)
    }

    // ***** Shielded trees *****

    async fn get_treestate(
        &self,
        id: super::types::BlockHash,
    ) -> BlockchainSourceResult<super::source::TreestateBytes> {
        let block = zaino_primitives::types::BlockHash::from(id.0);

        // Two reads: the serialized trees, and the roots. The interface reports
        // both per pool, and the port keeps them apart because they answer
        // different questions — one is the tree, the other its root.
        let (trees, roots) = tokio::try_join!(
            async {
                self.validator
                    .get_treestate_by_hash(block)
                    .await
                    .map_err(err)
            },
            async {
                self.validator
                    .get_commitment_tree_roots(block)
                    .await
                    .map_err(err)
            },
        )?;

        Ok((
            pool_treestate_slot(trees.sapling, roots.sapling),
            pool_treestate_slot(trees.orchard, roots.orchard),
            pool_treestate_slot(trees.ironwood, roots.ironwood),
        ))
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> BlockchainSourceResult<zaino_primitives::types::Treestate> {
        // The scaffolding takes an unparsed identifier; the port takes one or
        // the other, so it is resolved here.
        //
        // `final_root` stays absent on this path, unlike `get_treestate`: this
        // is the validator-passthrough fallback and no roots read accompanies
        // it, matching Zebra, whose own type documents the field as unused.
        match hash_or_height.parse::<u32>() {
            Ok(height) => self
                .validator
                .get_treestate(domain_height(height)?)
                .await
                .map_err(err),
            Err(_) => {
                let hash = zaino_primitives::types::BlockHash::from(parse_display_hash32(
                    &hash_or_height,
                )?);
                self.validator
                    .get_treestate_by_hash(hash)
                    .await
                    .map_err(err)
            }
        }
    }

    async fn get_subtree_roots(
        &self,
        pool: super::ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        let roots = self
            .validator
            .get_subtree_roots(domain_pool(pool), start_index, max_entries)
            .await
            .map_err(err)?;

        Ok(roots
            .into_iter()
            .map(|root| (<[u8; 32]>::from(root.root), root.end_height.into()))
            .collect())
    }

    async fn get_commitment_tree_roots(
        &self,
        id: super::types::BlockHash,
    ) -> BlockchainSourceResult<super::source::ShieldedTreeRoots> {
        let roots = self
            .validator
            .get_commitment_tree_roots(zaino_primitives::types::BlockHash::from(id.0))
            .await
            .map_err(err)?;

        Ok((
            roots.sapling.map(sapling_root).transpose()?,
            roots.orchard.map(orchard_root).transpose()?,
            roots.ironwood.map(orchard_root).transpose()?,
        ))
    }

    // ***** Node passthrough *****
    //
    // These forward validator-local facts that Zaino has no opinion about. The
    // ports model them as typed data rather than opaque JSON, so each method
    // here is a shape translation and nothing more.

    async fn get_info(&self) -> BlockchainSourceResult<zaino_primitives::types::rpc::NodeInfo> {
        self.validator.get_node_info().await.map_err(err)
    }

    async fn get_peer_info(
        &self,
    ) -> BlockchainSourceResult<Vec<zaino_primitives::types::rpc::PeerInfo>> {
        self.validator.get_peer_info().await.map_err(err)
    }

    async fn get_chain_tips(
        &self,
    ) -> BlockchainSourceResult<Vec<zaino_primitives::types::rpc::ChainTip>> {
        self.validator.get_chain_tips().await.map_err(err)
    }

    async fn get_mining_info(
        &self,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::MiningInfo> {
        self.validator.get_mining_info().await.map_err(err)
    }

    async fn get_block_subsidy(
        &self,
        height: u32,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::BlockSubsidy> {
        self.validator
            .get_block_subsidy(domain_height(height)?)
            .await
            .map_err(err)
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> BlockchainSourceResult<u64> {
        // Negative values are how this interface spells "use your default", so
        // they become absence rather than an error.
        let blocks = blocks.and_then(|b| u32::try_from(b).ok());
        let height = match height.and_then(|h| u32::try_from(h).ok()) {
            Some(h) => Some(domain_height(h)?),
            None => None,
        };

        self.validator
            .get_network_sol_ps(blocks, height)
            .await
            .map_err(err)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> BlockchainSourceResult<zaino_primitives::types::TransactionId> {
        let bytes = hex::decode(&raw_transaction_hex)
            .map_err(|e| invalid(format!("transaction is not hex: {e}")))?;

        self.validator
            .send_raw_transaction(bytes)
            .await
            .map_err(err)
    }

    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> BlockchainSourceResult<zaino_primitives::types::rpc::SpentInfo> {
        // Not `err`, which reports every domain rejection as `InvalidParameter`.
        // `getspentinfo` has two rejections that mean different things, and
        // zcashd gives one of them a specific code that clients key on.
        self.validator
            .get_spent_info(outpoint)
            .await
            .map_err(spent_info_err)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> BlockchainSourceResult<Option<zaino_primitives::types::rpc::TxOut>> {
        // `include_mempool` defaults to true, matching the interface: a caller
        // that does not say otherwise wants unconfirmed outputs counted.
        self.validator
            .get_tx_out(
                parse_display_txid(&txid)?,
                n,
                include_mempool.unwrap_or(true),
            )
            .await
            .map_err(err)
    }
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<super::source::NonfinalizedBlockReceiver>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        // Dropped by design: the block-carrying listener is replaced by tip and
        // block-arrival subscriptions, and the non-finalised state rework does
        // not use it. Reporting `None` is what a source without the capability
        // has always meant here.
        Ok(None)
    }

    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        self.validator.subscribe_to_blocks_received()
    }

    fn shutdown(&self) {
        self.validator.shutdown();
    }
}

impl ZebraValidatorSource {
    /// Build an RPC-only source from a validator endpoint.
    ///
    /// Skips the startup handshake that [`spawn_rpc`](Self::spawn_rpc) performs
    /// — no first-block wait, no network adoption — so the caller must already
    /// know the network and that the validator is serving. Intended for tests
    /// and for embedders that have done both themselves.
    pub fn rpc_only(
        rpc_address: &str,
        auth: Option<(String, String)>,
        network: zebra_chain::parameters::Network,
    ) -> Result<Self, BlockchainSourceError> {
        let client = zaino_rpc::RpcClient::new(zaino_rpc::RpcClientConfig {
            url: format!("http://{rpc_address}"),
            auth,
            ..Default::default()
        })
        .map_err(|e| invalid(format!("cannot build the validator RPC client: {e}")))?;

        Ok(Self::new(
            zaino_source_zebra::ZebraValidator::rpc_only(
                zaino_source_zebra_rpc::ZebraRpcAdapter::new(client),
            ),
            network,
            None,
        ))
    }

    /// Connect to a validator over JSON-RPC alone.
    ///
    /// Blocks until the validator can serve a tip. A freshly started validator
    /// answers RPC before it has committed its first block — Zebra serves
    /// `getblockchaininfo` and an empty mempool while `getbestblockhash` still
    /// reports no blocks, which can take minutes of peer discovery. Everything
    /// downstream assumes a servable tip, so waiting here is what stops spawn
    /// failing and exit-looping the whole process.
    pub async fn spawn_rpc(
        common: &crate::config::CommonBackendConfig,
    ) -> Result<
        (
            Self,
            zaino_primitives::types::rpc::NodeInfo,
            zebra_chain::parameters::Network,
        ),
        BlockchainSourceError,
    > {
        let adapter = rpc_adapter(common)?;
        let info = zaino_source::GetNodeInfo::get_node_info(&adapter)
            .await
            .map_err(err)?;

        while let Err(e) = zaino_source::GetChainTip::get_chain_tip(&adapter).await {
            tracing::info!(%e, "Waiting for validator to serve its first block");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        // Adopted before anything consumes a `Network`, so the index and its
        // validator cannot disagree about where an upgrade activates.
        let network = super::network_adoption::adopt_network(common, &rpc_adapter(common)?).await?;

        let validator = zaino_source_zebra::ZebraValidator::rpc_only(adapter);

        Ok((Self::new(validator, network.clone(), None), info, network))
    }

    /// Connect to a validator and additionally read its state database directly.
    ///
    /// Launches the chain syncer and waits for it to catch up before returning.
    /// The wait compares tip **hash** as well as height: the same height can
    /// name different blocks during a reorg, so height alone would report a
    /// false match and hand back a service that disagrees with the validator.
    pub async fn spawn_direct(
        common: &crate::config::CommonBackendConfig,
        direct: &crate::config::DirectConnectionConfig,
    ) -> Result<
        (
            Self,
            zaino_primitives::types::rpc::NodeInfo,
            zebra_chain::parameters::Network,
        ),
        BlockchainSourceError,
    > {
        use futures::TryFutureExt as _;
        use tower::{Service as _, ServiceExt as _};
        use zebra_state::{ReadRequest, ReadResponse};

        let adapter = rpc_adapter(common)?;
        let info = zaino_source::GetNodeInfo::get_node_info(&adapter)
            .await
            .map_err(err)?;

        let network = super::network_adoption::adopt_network(common, &rpc_adapter(common)?).await?;

        tracing::info!(
            grpc_address = %direct.validator_grpc_address,
            "Launching Chain Syncer"
        );
        let (mut read_state_service, _latest_chain_tip, chain_tip_change, sync_task_handle) =
            zebra_rpc::sync::init_read_state_with_syncer(
                direct.validator_state_config.clone(),
                &network,
                direct.validator_grpc_address,
            )
            .await
            .map_err(|e| invalid(e.to_string()))?
            .map_err(|e| invalid(e.to_string()))?;

        tracing::info!("Chain syncer launched");

        loop {
            let (validator_hash, validator_height) =
                zaino_source::GetChainTip::get_chain_tip(&adapter)
                    .await
                    .map_err(err)?;

            let response = read_state_service
                .ready()
                .and_then(|service| service.call(ReadRequest::Tip))
                .await
                .map_err(BlockchainSourceError::unrecoverable)?;

            let ReadResponse::Tip(tip) = response else {
                return Err(invalid("unexpected response to a Tip request".to_string()));
            };

            // As above: the syncer has no tip until genesis arrives, so this is
            // a wait rather than a failure.
            let Some((syncer_height, syncer_tip_hash)) = tip else {
                tracing::info!("Waiting for validator to serve its first block");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            };

            if u32::from(validator_height) == syncer_height.0
                && <[u8; 32]>::from(validator_hash) == syncer_tip_hash.0
            {
                tracing::info!(
                    height = syncer_height.0,
                    tip_hash = %syncer_tip_hash,
                    "ReadStateService synced with Zebra"
                );
                break;
            }

            tracing::info!(
                syncer_height = syncer_height.0,
                validator_height = u32::from(validator_height),
                syncer_tip_hash = %syncer_tip_hash,
                validator_tip_hash = ?validator_hash,
                "ReadStateService syncing with Zebra"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }

        let validator = zaino_source_zebra::ZebraValidator::with_read_state(
            adapter,
            zaino_source_zebra_readstate::ZebraReadStateAdapter::from_service(
                read_state_service,
                &network,
                Some(Arc::new(sync_task_handle)),
            ),
        );

        Ok((
            Self::new(validator, network.clone(), Some(chain_tip_change)),
            info,
            network,
        ))
    }
}

/// The JSON-RPC client for the new source stack.
///
/// Cookie auth is resolved to a credential pair here. The connector this
/// replaced read the cookie file once at construction and stored the token, and
/// `Basic` auth over `("__cookie__", token)` produces the identical header, so
/// this preserves that behaviour exactly rather than re-reading per request.
fn rpc_client(
    common: &crate::config::CommonBackendConfig,
) -> Result<zaino_rpc::RpcClient, BlockchainSourceError> {
    let auth = zaino_rpc::auth_from_parts(
        common.validator_cookie_path.as_deref(),
        Some(common.validator_rpc_user.clone()),
        Some(common.validator_rpc_password.clone()),
    )
    .map_err(|e| invalid(e.to_string()))?;

    zaino_rpc::RpcClient::new(zaino_rpc::RpcClientConfig {
        url: format!("http://{}", common.validator_rpc_address),
        auth,
        ..Default::default()
    })
    .map_err(|e| invalid(format!("cannot build the validator RPC client: {e}")))
}

/// The JSON-RPC adapter for the new source stack.
fn rpc_adapter(
    common: &crate::config::CommonBackendConfig,
) -> Result<zaino_source_zebra_rpc::ZebraRpcAdapter, BlockchainSourceError> {
    Ok(zaino_source_zebra_rpc::ZebraRpcAdapter::new(rpc_client(
        common,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This file's whole job is translating between two vocabularies, and a
    /// byte-order slip in either direction produces a plausible wrong answer
    /// rather than a failure. The fixture reads differently forwards and
    /// backwards so a mirrored result cannot pass by coincidence.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    /// The domain and zebra hash types hold the same internal order, so this
    /// conversion must *not* reverse — the opposite of `display_hex`.
    #[test]
    fn hash_conversion_preserves_internal_order() {
        let zebra = zebra_chain::block::Hash(ASYMMETRIC);

        assert_eq!(<[u8; 32]>::from(hash(zebra)), ASYMMETRIC);
    }

    /// The domain height type enforces the protocol maximum; this crate's zebra
    /// heights do not. The boundary is where an impossible height has to be
    /// caught rather than silently truncated.
    #[test]
    fn heights_cross_the_boundary_and_reject_impossible_values() {
        let ok = height(zebra_chain::block::Height(1_234_567)).expect("within range");
        assert_eq!(u32::from(ok), 1_234_567);

        assert!(
            height(zebra_chain::block::Height(u32::MAX)).is_err(),
            "a height above the protocol maximum must not cross silently"
        );
        assert!(domain_height(u32::MAX).is_err());
    }

    /// A domain rejection and a transport fault are different facts. The
    /// scaffolding error cannot represent the distinction structurally, so it
    /// must survive in the message rather than being flattened away.
    #[test]
    fn error_flattening_keeps_the_failure_kind() {
        let domain: QueryError<zaino_source::GetChainTipError> =
            QueryError::Domain(zaino_source::GetChainTipError::NotReady);
        let transport: QueryError<zaino_source::GetChainTipError> = QueryError::Fetch(
            zaino_source::FetchError::new(zaino_source::FailureMode::Connection, "refused"),
        );

        assert!(err(domain).to_string().contains("rejected"));
        assert!(err(transport).to_string().contains("unreachable"));
    }

    /// Blocks cross the port as canonical bytes so that consumers building
    /// their own representation get exactly what the hash commits to. Garbage
    /// must fail loudly here rather than produce a half-formed block.
    #[test]
    fn malformed_block_bytes_are_rejected() {
        assert!(block_from_bytes(vec![0x00, 0x01, 0x02]).is_err());
    }

    /// The inverse of the display-order parsers under test.
    ///
    /// Production rendering moved to `zaino-serve`'s wire module along with the
    /// responses that need it; what remains here is parsing. This oracle stays
    /// so the parsers can still be pinned against a round trip.
    fn display_hex(mut bytes: [u8; 32]) -> String {
        bytes.reverse();
        hex::encode(bytes)
    }

    /// A txid arrives from the interface in display order and must reach the
    /// port in internal order. Paired with `display_hex`, the two must compose
    /// to the identity — otherwise a lookup silently queries a different
    /// transaction.
    #[test]
    fn txid_parse_and_render_are_inverses() {
        let mut display_order = ASYMMETRIC;
        display_order.reverse();
        let as_written = hex::encode(display_order);

        let parsed = parse_display_txid(&as_written).expect("valid txid");
        assert_eq!(
            <[u8; 32]>::from(parsed),
            ASYMMETRIC,
            "parsing did not restore internal order"
        );
        assert_eq!(
            display_hex(<[u8; 32]>::from(parsed)),
            as_written,
            "render(parse(x)) must be x"
        );
    }

    #[test]
    fn txid_parse_rejects_malformed_input() {
        assert!(parse_display_txid("not hex").is_err());
        assert!(
            parse_display_txid("aabb").is_err(),
            "a short txid must not be silently accepted"
        );
    }

    /// The pool enums share a name and their variants but not a role, so the
    /// mapping is written by hand. A transposition here would query the wrong
    /// pool's subtrees and return plausible-looking wrong roots.
    #[test]
    fn shielded_pool_mapping_is_not_transposed() {
        use crate::chain_index::ShieldedPool as Ours;
        use zaino_primitives::types::ShieldedPool as Theirs;

        assert_eq!(domain_pool(Ours::Sapling), Theirs::Sapling);
        assert_eq!(domain_pool(Ours::Orchard), Theirs::Orchard);
        assert_eq!(domain_pool(Ours::Ironwood), Theirs::Ironwood);
    }

    /// Block identifiers arrive from the interface in display order. This is
    /// the same contract as `parse_display_txid`, and the two must not drift
    /// apart.
    #[test]
    fn hash32_parse_restores_internal_order() {
        let mut display_order = ASYMMETRIC;
        display_order.reverse();

        let parsed = parse_display_hash32(&hex::encode(display_order)).expect("valid hash");

        assert_eq!(parsed, ASYMMETRIC);
        assert_eq!(display_hex(parsed), hex::encode(display_order));
    }

    #[test]
    fn hash32_parse_rejects_wrong_length() {
        assert!(parse_display_hash32("aabbcc").is_err());
        assert!(parse_display_hash32("zz".repeat(32).as_str()).is_err());
    }

    /// A root that is 32 bytes but not a point on the pool's curve means the
    /// validator sent something impossible. That must surface rather than be
    /// defaulted into a valid-looking root.
    #[test]
    fn a_tree_root_that_is_not_a_curve_point_is_rejected() {
        let impossible = zaino_primitives::types::TreeRootInfo {
            root: zaino_primitives::types::TreeRoot::new([0xff; 32]),
            size: 1,
        };

        assert!(sapling_root(impossible.clone()).is_err());
        assert!(orchard_root(impossible).is_err());
    }
}

/// Clamps a `getaddressdeltas` height range to the current best tip.
///
/// The interface's range is open-ended in a way the port's is not: `end == 0`
/// means "to the tip", and either bound may name a height the chain has not
/// reached. Both clamp down to the tip, so a caller asking past the end of the
/// chain gets the chain rather than an error — and, in particular, the
/// single-address form (which carries no range at all, and so arrives as
/// `0..=0`) means the whole chain rather than genesis alone.
///
/// Total, and the clamp is what makes it so. A bound only reaches
/// [`Height::try_from`](zaino_primitives::types::Height) when it is at or below
/// `tip`, and `tip` is already a `Height` — so the value is in range by
/// construction and the conversion cannot fail. This used to return a `Result`
/// whose `Err` was unreachable; a `Result` nobody can provoke is a test that
/// can never be written, so the totality is expressed in the signature instead.
fn clamp_deltas_range_to_tip(
    tip: zaino_primitives::types::Height,
    start_raw: u32,
    end_raw: u32,
) -> (
    zaino_primitives::types::Height,
    zaino_primitives::types::Height,
) {
    let end = if end_raw == 0 {
        tip
    } else {
        clamp_to_tip(end_raw, tip)
    };
    (clamp_to_tip(start_raw, tip), end)
}

/// A raw height clamped to `tip`, hence always a valid
/// [`Height`](zaino_primitives::types::Height).
fn clamp_to_tip(raw: u32, tip: zaino_primitives::types::Height) -> zaino_primitives::types::Height {
    match zaino_primitives::types::Height::try_from(raw) {
        Ok(height) => height.min(tip),
        // Above the protocol maximum is above any tip, so the clamp applies
        // just as it does to a merely-too-large value. Not an error case.
        Err(_) => tip,
    }
}

/// Maps one pool's reported tree and root onto the interface's treestate slot.
///
/// A pool is reported only when it has a tree at this block, so an absent
/// `state` is an absent slot rather than an empty tree — emitting a serialized
/// empty tree would claim the pool is active when it is not, which is what
/// `z_gettreestate` keys on to omit pre-activation pools. The root is attached
/// only when the validator reported one; Zebra does not, so the field is
/// genuinely absent rather than zeroed.
fn pool_treestate_slot(
    pool: Option<zaino_primitives::types::PoolTreestate>,
    root: Option<zaino_primitives::types::TreeRootInfo>,
) -> Option<zaino_primitives::types::PoolTreestate> {
    // The tree port answers with the root absent — see its adapters — so the
    // root read is what fills it. Both are held in internal byte order; the
    // display-order reversal `z_gettreestate` wants happens at the wire.
    pool.map(|pool| zaino_primitives::types::PoolTreestate {
        final_root: root.map(|info| info.root),
        ..pool
    })
}

#[cfg(test)]
mod pool_treestate_slot_tests {
    use super::pool_treestate_slot;
    use zaino_primitives::types::{TreeRoot, TreeRootInfo};

    fn root_info(byte: u8) -> TreeRootInfo {
        TreeRootInfo {
            root: TreeRoot::from([byte; 32]),
            size: 1,
        }
    }

    /// The validator's root must reach the slot. Zebra populates a root for
    /// every pool it serves, and dropping it left every `z_gettreestate`
    /// response with no `finalRoot` for any pool.
    ///
    /// The root now stays in internal byte order here; the display-order
    /// reversal `z_gettreestate` wants happens at the wire boundary, and is
    /// pinned there.
    #[test]
    fn final_root_passes_through() {
        let slot = pool_treestate_slot(
            Some(zaino_primitives::types::PoolTreestate {
                final_root: None,
                final_state: vec![1u8, 2, 3],
            }),
            Some(root_info(7)),
        )
        .expect("a reported tree maps to a populated slot");

        assert_eq!(slot.final_state, vec![1u8, 2, 3]);
        assert_eq!(
            slot.final_root,
            Some(zaino_primitives::types::TreeRoot::from([7u8; 32])),
            "the validator's root must pass through to the treestate slot"
        );
    }

    /// Roots stay in internal byte order here. `z_gettreestate` writes
    /// `finalRoot` in display order, but that reversal is the wire boundary's —
    /// see `zaino-serve`'s `wire::treestate::final_root_is_written_in_display_order`.
    /// Reversing in both places, or in neither, would emit valid-looking hex
    /// naming a root that does not exist.
    #[test]
    fn root_keeps_internal_byte_order() {
        let mut internal = [0u8; 32];
        internal[0] = 0xaa;
        internal[31] = 0xbb;

        let slot = pool_treestate_slot(
            Some(zaino_primitives::types::PoolTreestate {
                final_root: None,
                final_state: vec![0],
            }),
            Some(TreeRootInfo {
                root: TreeRoot::from(internal),
                size: 1,
            }),
        )
        .expect("a reported tree maps to a populated slot");

        let emitted = <[u8; 32]>::from(slot.final_root.expect("a reported root reaches the slot"));
        assert_eq!(
            emitted, internal,
            "the reversal belongs at the wire, not here"
        );
    }

    /// A pool the validator did not report stays absent — for ironwood this is
    /// every height before NU6.3, where back-filling an empty tree would emit
    /// the field on networks that never activate it.
    #[test]
    fn absent_tree_maps_to_absent_slot() {
        assert_eq!(pool_treestate_slot(None, None), None);
        assert_eq!(
            pool_treestate_slot(None, Some(root_info(7))),
            None,
            "a root without a tree is not a pool this block has"
        );
    }

    /// A tree without a root is still a pool this block has; the root is
    /// reported absent rather than zeroed.
    #[test]
    fn tree_without_root_keeps_the_slot() {
        let slot = pool_treestate_slot(
            Some(zaino_primitives::types::PoolTreestate {
                final_root: None,
                final_state: vec![9u8],
            }),
            None,
        )
        .expect("a reported tree is a slot");
        assert_eq!(slot.final_root, None);
    }
}

#[cfg(test)]
mod error_source_chain {
    use super::*;

    /// `zaino-serve` recovers zcashd-compatible RPC error codes by
    /// downcast-walking [`std::error::Error::source`] chains (see
    /// `getblock_error_object_from_indexer_error` and
    /// `sendrawtransaction_error_object_from_indexer_error` in
    /// `zaino-serve/src/rpc/jsonrpc/service.rs`). This boundary must therefore
    /// preserve the typed cause instead of flattening it to a string, or
    /// failures surface as generic internal errors rather than the legacy codes
    /// lightwalletd-family clients key on.
    /// A domain rejection must reach the serving layer with a code too.
    ///
    /// Before the adapters learned to distinguish a missing object from an
    /// unreachable node, "block not found" arrived here as a `Fetch` error
    /// carrying `-8`, and `zaino-serve` recovered that code. Now it arrives as
    /// a `Domain` rejection, so this boundary must attach the code itself —
    /// otherwise the reclassification would silently downgrade every
    /// not-found response to a generic internal error.
    #[test]
    fn domain_rejection_carries_a_legacy_code_through_source() {
        let flattened = err::<zaino_source::GetBlockError>(QueryError::Domain(
            zaino_source::GetBlockError::HeightNotFound(
                zaino_primitives::types::Height::try_from(42u32).expect("valid height"),
            ),
        ));

        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&flattened);
        let mut code = None;
        while let Some(source) = current {
            if let Some(rejection) = source.downcast_ref::<crate::error::LegacyRpcError>() {
                code = Some(rejection.code);
                break;
            }
            current = source.source();
        }

        assert_eq!(
            code,
            Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
            "a domain rejection must stay downcastable to its legacy code"
        );
    }

    /// Recovers the legacy code a rejection carries, the way `zaino-serve`
    /// does: by walking the `source()` chain and downcasting.
    fn recovered_code(error: &BlockchainSourceError) -> Option<i64> {
        std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
            error.source()
        })
        .find_map(|source| {
            source
                .downcast_ref::<crate::error::LegacyRpcError>()
                .map(|rejection| rejection.code)
        })
    }

    /// zcashd answers "no spend on record" with `-5 Unable to get spent info`,
    /// and that pair is what a client matches on. Neither this method's first
    /// rewrite nor the connector it replaced actually served it — the former
    /// reported `-8`, the latter consumed the code into a typed error and
    /// served a generic internal error. Both lost the only part the client uses.
    #[test]
    fn an_unspent_output_reports_zcashds_own_code() {
        let rejected = spent_info_err(QueryError::Domain(
            zaino_source::GetSpentInfoError::NotSpent,
        ));

        assert_eq!(
            recovered_code(&rejected),
            Some(zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64)
        );
        assert!(
            rejected.to_string().contains("Unable to get spent info"),
            "zcashd's message travels with its code: {rejected}"
        );
    }

    /// `getspentinfo` is zcashd-only, so a zebrad-backed deployment cannot
    /// answer it at all. That has to stay distinguishable from "unspent" all
    /// the way to the wire — served as `-5`, it would tell every client that
    /// every output is unspent.
    #[test]
    fn an_unsupported_method_does_not_masquerade_as_unspent() {
        let rejected = spent_info_err(QueryError::Domain(
            zaino_source::GetSpentInfoError::Unsupported,
        ));

        assert_eq!(recovered_code(&rejected), Some(METHOD_NOT_FOUND));
        assert_ne!(
            recovered_code(&rejected),
            Some(zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64)
        );
    }

    /// A transport fault is not an answer about the outpoint, so it must keep
    /// the typed `FetchError` rather than acquiring a legacy code that would
    /// tell the client something about the output.
    #[test]
    fn a_transport_fault_on_spent_info_stays_a_fetch_failure() {
        let rejected = spent_info_err(QueryError::Fetch(zaino_source::FetchError::new(
            zaino_source::FailureMode::Connection,
            "refused",
        )));

        assert_eq!(recovered_code(&rejected), None);
        assert!(rejected.to_string().contains("unreachable"));
    }

    #[tokio::test]
    async fn transport_error_stays_reachable_through_source() {
        // Port 1 refuses connections, so the request fails at the transport
        // layer without contacting any validator.
        let source = ZebraValidatorSource::rpc_only(
            "127.0.0.1:1",
            None,
            zebra_chain::parameters::Network::new_regtest(Default::default()),
        )
        .expect("client construction is network-free");

        let error = BlockchainSource::get_best_block_hash(&source)
            .await
            .expect_err("a request to a closed port must fail");

        let reached = std::iter::successors(
            Some(&error as &(dyn std::error::Error + 'static)),
            |error| error.source(),
        )
        .any(|error| error.downcast_ref::<zaino_source::FetchError>().is_some());

        assert!(
            reached,
            "the typed FetchError must stay reachable via the source() chain; \
             stringifying it strips the FailureMode the serve layer recovers"
        );
    }
}

#[cfg(test)]
mod clamp_deltas_range_to_tip_tests {
    use super::clamp_deltas_range_to_tip;
    use zaino_primitives::types::Height;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// `end == 0` means "to the tip", and both bounds clamp down to it.
    #[test]
    fn bounds_clamp_to_tip() {
        let tip = height(100);

        assert_eq!(clamp_deltas_range_to_tip(tip, 5, 0), (height(5), tip));
        assert_eq!(clamp_deltas_range_to_tip(tip, 5, 400), (height(5), tip));
        assert_eq!(clamp_deltas_range_to_tip(tip, 300, 50), (tip, height(50)));
    }

    /// The clamp is what makes this total, so the value that would otherwise
    /// fail conversion has to land on the tip like any other over-large bound.
    ///
    /// This replaces `absent_tip_is_a_typed_error`, which the port's move made
    /// unrepresentable: `get_best_block_height` returns a `Height`, not an
    /// `Option<Height>`, so a tip cannot be absent here any more. The failure
    /// mode that remained worth pinning is the one at the other end.
    #[test]
    fn a_bound_past_the_protocol_maximum_clamps_rather_than_failing() {
        let tip = height(100);

        assert_eq!(
            clamp_deltas_range_to_tip(tip, u32::MAX, u32::MAX),
            (tip, tip)
        );
    }

    /// The single-address form carries no range, so it arrives as `0..=0` and
    /// must mean the whole chain. Forwarding it verbatim would report only
    /// genesis — the regression this clamp exists to prevent.
    #[test]
    fn absent_range_covers_the_whole_chain() {
        assert_eq!(
            clamp_deltas_range_to_tip(height(100), 0, 0),
            (height(0), height(100)),
            "an absent range must span the chain, not genesis alone"
        );
    }

    /// A range already inside the chain is untouched.
    #[test]
    fn in_range_bounds_pass_through() {
        assert_eq!(
            clamp_deltas_range_to_tip(height(100), 10, 20),
            (height(10), height(20))
        );
    }
}
