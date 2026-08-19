//! Trait implementations: zaino-source query traits on [`ZebraRpcAdapter`].

use zaino_primitives::types::{Block, BlockHash, ChainMetadata, Height, TransactionId, Treestate};
use zaino_rpc::RpcClient;
use zaino_source::{
    FailureMode, FetchError, GetBlockError, GetChainTipError, GetTreestateError, QueryError,
};
use zebra_chain::serialization::ZcashDeserializeInto;

use crate::parse;

/// Zebra JSON-RPC adapter.
///
/// Implements zaino-source query traits by delegating to an [`RpcClient`],
/// deserializing via `zebra-chain`, and converting to domain types.
/// Single-attempt — wrap with [`zaino_source::Resilient`] for retries.
pub struct ZebraRpcAdapter {
    rpc: RpcClient,
}

impl ZebraRpcAdapter {
    /// Wrap an existing [`RpcClient`].
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }
}

/// Parse errors are always non-retryable.
fn from_parse(e: parse::ParseError) -> FetchError {
    FetchError::new(FailureMode::Parse, e.to_string())
}

/// The RPC error codes a validator uses to say "the thing you asked about does
/// not exist".
///
/// - `-8` (`InvalidParameter`) is zebrad's code for `getblock`'s literal
///   `"Block not found"`.
/// - `-5` (`InvalidAddressOrKey`) is its code for the same answer on the
///   hash-addressed methods, and for `getrawtransaction`'s "no information
///   about transaction".
///
/// # Why reading these as "absent" is safe here
///
/// Both codes are overloaded upstream: `-8` also means "your parameter was
/// malformed", `-5` also means "that address is not valid". Reading them as
/// absence would be dangerous if a caller could send a malformed parameter —
/// a Zaino bug would surface as a silent `None` instead of an error.
///
/// It cannot, on the methods that use [`absent_or_fetch`]. Every parameter
/// there is rendered from a domain type: a [`Height`] becomes a decimal `u32`,
/// a [`BlockHash`] or [`TransactionHash`] becomes exactly 64 hex characters,
/// and the verbosity arguments are literals in this file. None of those can be
/// malformed, so on those methods these codes have one meaning.
///
/// The address-keyed methods are the exception and are handled separately: for
/// them `-5` *is* the caller's address being rejected, which is why their
/// domain error says so rather than saying "not found".
const NOT_FOUND_CODES: [i64; 2] = [-5, -8];

/// The code a validator uses to reject a caller's address.
///
/// `-5` (`InvalidAddressOrKey`) only. `-8` is deliberately excluded here: on
/// the address-keyed methods it means the *request envelope* was malformed —
/// a bad `start`/`end` range, or a missing field — which is a Zaino bug, not
/// the caller's address being wrong, and must surface as a failure.
const INVALID_ADDRESS_CODE: i64 = -5;

/// `getspentinfo`'s answer for an outpoint with no spend on record.
///
/// The same `-5` as [`INVALID_ADDRESS_CODE`], named separately because it says
/// something different: on `getspentinfo` there is no address to reject, and
/// zcashd uses this code for "unspent, unknown, or no spent index" alike.
const NO_SPEND_ON_RECORD: i64 = -5;

/// The JSON-RPC standard code for a method the server does not implement.
///
/// Not a zcashd legacy code — it comes from the envelope, not the application.
/// Relevant because `getspentinfo` is zcashd-only, so a zebrad-backed
/// deployment answers every call with this.
const METHOD_NOT_FOUND: i64 = -32601;

/// Whether an error is the validator rejecting one of the addresses asked about.
fn is_invalid_address(error: &FetchError) -> bool {
    matches!(error.mode, FailureMode::RpcError(INVALID_ADDRESS_CODE))
}

/// Whether an error is the validator answering "that does not exist".
fn is_not_found(error: &FetchError) -> bool {
    matches!(error.mode, FailureMode::RpcError(code) if NOT_FOUND_CODES.contains(&code))
}

/// Classifies a transport failure as either the port's "absent" domain answer
/// or a genuine fetch failure.
///
/// The distinction is load-bearing, not cosmetic: [`QueryError::Domain`] is an
/// answer and is returned immediately, while [`QueryError::Fetch`] is a failure
/// and is retried by [`Resilient`](zaino_source::Resilient) and escalated by
/// consumers. A missing block reported as a fetch failure stalls the sync loop
/// against a healthy validator, which is exactly what it did before this
/// existed.
fn absent_or_fetch<E>(error: zaino_rpc::RpcError, absent: impl FnOnce() -> E) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    let error: FetchError = error.into();
    if is_not_found(&error) {
        QueryError::Domain(absent())
    } else {
        QueryError::Fetch(error)
    }
}

/// The codes a validator uses to reject a submitted transaction.
///
/// `-22` (`Deserialization`) is the transaction failing to parse; `-25`
/// (`Verify`), `-26` (`VerifyRejected`) and `-27` (`VerifyAlreadyInChain`) are
/// the validator considering it and declining. All four are answers about the
/// transaction, not failures to reach the node, so a client should see the
/// reason rather than a generic transport error.
fn submission_rejection(error: &FetchError) -> Option<zaino_source::SendRawTransactionError> {
    use zaino_source::SendRawTransactionError as Rejection;

    match error.mode {
        FailureMode::RpcError(-22) => Some(Rejection::Malformed(error.message.clone())),
        FailureMode::RpcError(-27..=-25) => Some(Rejection::Rejected(error.message.clone())),
        _ => None,
    }
}

/// The codes `getspentinfo` answers with rather than fails with.
///
/// `-5` is zcashd saying it has no spend on record; `-32601` is a validator
/// saying it does not implement the method, which for this zcashd-only method
/// means the backing node is zebrad. Both are answers about the question, so a
/// client should see the reason rather than a generic transport error — and
/// they must stay distinct, because reading "I cannot answer" as "the output is
/// unspent" would report a spent output as unspent.
fn spent_info_rejection(error: &FetchError) -> Option<zaino_source::GetSpentInfoError> {
    use zaino_source::GetSpentInfoError as Rejection;

    match error.mode {
        FailureMode::RpcError(NO_SPEND_ON_RECORD) => Some(Rejection::NotSpent),
        FailureMode::RpcError(METHOD_NOT_FOUND) => Some(Rejection::Unsupported),
        _ => None,
    }
}

/// Classifies a transport failure on a mempool listing method.
///
/// `-32601` is the validator saying it does not implement the method, which on
/// `getrawmempool` means the backing node exposes no mempool at all. That is an
/// answer about the node rather than a failure of this request: retrying cannot
/// change it, so a consumer should be told rather than left to re-poll a
/// validator that will never answer.
///
/// Nothing else is classified. A mempool that is merely empty answers with an
/// empty list, and a validator still starting up fails at the transport level —
/// both of which are correctly *not* this.
fn mempool_unavailable_or_fetch<E>(
    error: zaino_rpc::RpcError,
    unavailable: impl FnOnce() -> E,
) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    let error: FetchError = error.into();
    if matches!(error.mode, FailureMode::RpcError(METHOD_NOT_FOUND)) {
        QueryError::Domain(unavailable())
    } else {
        QueryError::Fetch(error)
    }
}

/// Classifies a transport failure on an address-keyed method.
///
/// Separate from [`absent_or_fetch`] because these methods reject the caller's
/// *address*, not a missing object, and only `-5` says so — see
/// [`INVALID_ADDRESS_CODE`].
fn invalid_address_or_fetch<E>(
    error: zaino_rpc::RpcError,
    invalid: impl FnOnce(String) -> E,
) -> QueryError<E>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    let error: FetchError = error.into();
    if is_invalid_address(&error) {
        QueryError::Domain(invalid(error.message))
    } else {
        QueryError::Fetch(error)
    }
}

impl zaino_source::GetBlock for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        // Fetch raw hex block via getblock(height, 0).
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(0.into()),
        ];
        let value =
            self.rpc.call("getblock", params).await.map_err(|error| {
                absent_or_fetch(error, || GetBlockError::HeightNotFound(height))
            })?;

        // Hex decode.
        let raw_bytes = parse::parse_raw_block(&value).map_err(from_parse)?;

        // Deserialize via zebra-chain.
        let zebra_block: zebra_chain::block::Block = raw_bytes
            .zcash_deserialize_into()
            .map_err(|e| from_parse(parse::ParseError::Deserialize(e.to_string())))?;

        // Cumulative tree sizes are indexed state, not present in the block
        // bytes, so they are zero here and populated by the caller that tracks
        // them (via `GetTreestate` or its own index). Zero is a placeholder,
        // not a measurement: a consumer that needs real sizes must not read
        // them off this block.
        let chain_metadata = ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        };

        zaino_convert_zebra::block_from_zebra(&zebra_block, chain_metadata)
            .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()).into())
    }
}

impl zaino_source::GetChainTip for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let hash_value = self
            .rpc
            .call("getbestblockhash", vec![])
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        let hash = parse::parse_block_hash(&hash_value).map_err(from_parse)?;

        let height_value = self
            .rpc
            .call("getblockcount", vec![])
            .await
            .map_err(|e| QueryError::Fetch(e.into()))?;
        let height = parse::parse_height(&height_value).map_err(from_parse)?;

        Ok((hash, height))
    }
}

impl zaino_source::GetPreIndexCompactBlock for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::PreIndexCompactBlock, QueryError<GetBlockError>> {
        // RPC returns full block bytes — no way to request compact from the validator.
        // We full-deserialize via zebra-chain then convert to our compact type.
        // The savings vs get_block is skipping the domain Block intermediate —
        // we go zebra Block → compact directly.
        //
        // TODO: once compact_deserialize supports streaming (Reader instead of
        // &[u8]), we can skip the full zebra deserialize on this path too.
        use zaino_source::GetBlock;
        let block = self.get_block(height).await?;
        Ok(zaino_primitives::types::PreIndexCompactBlock::from(&block))
    }
}

impl zaino_source::GetTreestate for ZebraRpcAdapter {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self), fields(h = u32::from(height))))]
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        let params = vec![serde_json::Value::String(u32::from(height).to_string())];
        let value = self
            .rpc
            .call("z_gettreestate", params)
            .await
            .map_err(|error| {
                absent_or_fetch(error, || GetTreestateError::HeightNotFound(height))
            })?;
        parse::parse_treestate(&value).map_err(|e| from_parse(e).into())
    }
}

/// Build the positional params for a hash-addressed call.
fn hash_param(hash: BlockHash) -> Vec<serde_json::Value> {
    vec![serde_json::Value::String(hash_to_display_hex(hash))]
}

/// Render a block hash in RPC display order (big-endian hex).
fn hash_to_display_hex(hash: BlockHash) -> String {
    let mut bytes = <[u8; 32]>::from(hash);
    bytes.reverse();
    hex::encode(bytes)
}

/// Render a transaction id in RPC display order (big-endian hex).
fn txid_to_display_hex(txid: TransactionId) -> String {
    let mut bytes = <[u8; 32]>::from(txid);
    bytes.reverse();
    hex::encode(bytes)
}

fn addresses_param(addresses: Vec<String>) -> serde_json::Value {
    serde_json::json!({ "addresses": addresses })
}

impl ZebraRpcAdapter {
    /// Issue a call and parse its result, classifying transport failures with
    /// `classify` and parse failures as [`QueryError::Fetch`].
    ///
    /// The classifier is what separates the wrappers below: it decides whether a
    /// given transport failure is the port's domain answer or a real fetch
    /// failure. `timeout` is the per-request bound, `None` for the client's
    /// default.
    async fn call_parsed_classified<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        timeout: Option<std::time::Duration>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
        classify: impl FnOnce(zaino_rpc::RpcError) -> QueryError<E>,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let value = self
            .rpc
            .call_with_timeout(method, params, timeout)
            .await
            .map_err(classify)?;
        parse(&value).map_err(|e| QueryError::Fetch(from_parse(e)))
    }

    /// Issue a call and parse its result, mapping transport and parse failures
    /// into the caller's error type.
    async fn call_parsed<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        self.call_parsed_classified(method, params, None, parse, |error| {
            QueryError::Fetch(error.into())
        })
        .await
    }

    /// [`call_parsed`](Self::call_parsed), but reporting the validator's
    /// not-found codes as the port's own "absent" answer.
    ///
    /// For the methods whose port defines one — see [`NOT_FOUND_CODES`] for why
    /// reading those codes this way is unambiguous on exactly these methods.
    async fn call_parsed_or_absent<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
        absent: impl FnOnce() -> E,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        self.call_parsed_classified(method, params, None, parse, |error| {
            absent_or_fetch(error, absent)
        })
        .await
    }

    /// [`call_parsed`](Self::call_parsed), for the address-keyed methods,
    /// reporting the validator's address rejection as the port's own
    /// `InvalidAddress` answer.
    async fn call_parsed_or_invalid_address<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<T, parse::ParseError>,
        invalid: impl FnOnce(String) -> E,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        self.call_parsed_classified(method, params, None, parse, |error| {
            invalid_address_or_fetch(error, invalid)
        })
        .await
    }

    /// [`call_parsed`](Self::call_parsed), but reporting the validator's
    /// not-found codes as `Ok(None)`.
    ///
    /// For `gettxout`, whose *ordinary* answer is already optional: an unspent
    /// output is a successful query with nothing to report. zcashd returns JSON
    /// `null` and zebrad returns a not-found code; both mean absent.
    ///
    /// `getspentinfo` used to share this, and should not have: it has no null
    /// answer in the interface, so absence is a domain rejection carrying its
    /// own code rather than an `Ok(None)`. See
    /// [`GetSpentInfo`](zaino_source::GetSpentInfo).
    async fn call_parsed_optional<T, E>(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
        parse: impl FnOnce(&serde_json::Value) -> Result<Option<T>, parse::ParseError>,
    ) -> Result<Option<T>, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        match self.rpc.call(method, params).await {
            Ok(value) => parse(&value).map_err(|e| QueryError::Fetch(from_parse(e))),
            Err(error) => {
                let error: FetchError = error.into();
                if is_not_found(&error) {
                    Ok(None)
                } else {
                    Err(QueryError::Fetch(error))
                }
            }
        }
    }
}

impl zaino_source::GetBlockByHash for ZebraRpcAdapter {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<zaino_source::GetBlockByHashError>> {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Number(0.into()),
        ];
        let raw_bytes: Vec<u8> = self
            .call_parsed_or_absent("getblock", params, parse::parse_raw_block, || {
                zaino_source::GetBlockByHashError::NotFound(hash)
            })
            .await?;

        let zebra_block: zebra_chain::block::Block = raw_bytes
            .zcash_deserialize_into()
            .map_err(|e| from_parse(parse::ParseError::Deserialize(e.to_string())))?;

        // Tree sizes are indexed state, not block data — see `GetBlock`.
        let chain_metadata = ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        };
        zaino_convert_zebra::block_from_zebra(&zebra_block, chain_metadata)
            .map_err(|e| FetchError::new(FailureMode::Parse, e.to_string()).into())
    }
}

impl zaino_source::GetBestBlockHeight for ZebraRpcAdapter {
    async fn get_best_block_height(
        &self,
    ) -> Result<Height, QueryError<zaino_source::GetBestBlockHeightError>> {
        self.call_parsed("getblockcount", vec![], parse::parse_height)
            .await
    }
}

impl zaino_source::GetBlockVerbose for ZebraRpcAdapter {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::BlockVerbose, QueryError<zaino_source::GetBlockVerboseError>>
    {
        // Verbosity 1 rather than 2: this reads only chain-state facts, none of
        // which live in the transaction list, so asking for full transaction
        // objects would cost the validator work the answer discards.
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(1.into()),
        ];
        self.call_parsed_or_absent("getblock", params, parse::parse_block_verbose, || {
            zaino_source::GetBlockVerboseError::HeightNotFound(height)
        })
        .await
    }
}

impl zaino_source::GetBlockVerboseByHash for ZebraRpcAdapter {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<zaino_primitives::types::BlockVerbose, QueryError<zaino_source::GetBlockVerboseError>>
    {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Number(1.into()),
        ];
        self.call_parsed_or_absent("getblock", params, parse::parse_block_verbose, || {
            zaino_source::GetBlockVerboseError::BlockNotFound(hash)
        })
        .await
    }
}

impl zaino_source::GetBlockHeader for ZebraRpcAdapter {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockHeaderVerbose,
        QueryError<zaino_source::GetBlockHeaderError>,
    > {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Bool(true),
        ];
        self.call_parsed_or_absent(
            "getblockheader",
            params,
            parse::parse_block_header_verbose,
            || zaino_source::GetBlockHeaderError::BlockNotFound(hash),
        )
        .await
    }
}

impl zaino_source::GetRawBlockHeader for ZebraRpcAdapter {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockHeaderError>> {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Bool(false),
        ];
        self.call_parsed_or_absent("getblockheader", params, parse::parse_raw_block, || {
            zaino_source::GetBlockHeaderError::BlockNotFound(hash)
        })
        .await
    }
}

impl zaino_source::GetBlockDeltas for ZebraRpcAdapter {
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockDeltas,
        QueryError<zaino_source::GetBlockDeltasError>,
    > {
        self.call_parsed_or_absent(
            "getblockdeltas",
            hash_param(hash),
            parse::parse_block_deltas,
            || zaino_source::GetBlockDeltasError::BlockNotFound(hash),
        )
        .await
    }
}

impl zaino_source::GetChainTips for ZebraRpcAdapter {
    async fn get_chain_tips(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::ChainTip>,
        QueryError<zaino_source::GetChainTipsError>,
    > {
        self.call_parsed("getchaintips", vec![], parse::parse_chain_tips)
            .await
    }
}

impl zaino_source::GetDifficulty for ZebraRpcAdapter {
    async fn get_difficulty(
        &self,
    ) -> Result<zaino_primitives::types::Difficulty, QueryError<zaino_source::GetDifficultyError>>
    {
        self.call_parsed("getdifficulty", vec![], parse::as_f64)
            .await
    }
}

impl zaino_source::GetBlockchainInfo for ZebraRpcAdapter {
    async fn get_blockchain_info(
        &self,
    ) -> Result<
        zaino_primitives::types::BlockchainInfo,
        QueryError<zaino_source::GetBlockchainInfoError>,
    > {
        self.call_parsed("getblockchaininfo", vec![], parse::parse_blockchain_info)
            .await
    }
}

impl zaino_source::GetMempoolTxids for ZebraRpcAdapter {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<zaino_source::GetMempoolTxidsError>> {
        self.call_parsed_classified(
            "getrawmempool",
            vec![],
            None,
            parse::parse_mempool_txids,
            |error| {
                mempool_unavailable_or_fetch(error, || {
                    zaino_source::GetMempoolTxidsError::Unavailable
                })
            },
        )
        .await
    }
}

impl zaino_source::GetMempoolMetadata for ZebraRpcAdapter {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<zaino_source::MempoolTxMeta>, QueryError<zaino_source::GetMempoolMetadataError>>
    {
        // The validator answers this by walking its entire mempool, loading
        // full transactions and aggregating descendant stats. Under the
        // client-wide timeout a busy validator would read as a hard error, so
        // this method names its own bound rather than inheriting the default —
        // the only axis on which it differs from an ordinary call.
        self.call_parsed_classified(
            "getrawmempool",
            vec![serde_json::Value::Bool(true)],
            Some(zaino_rpc::HEAVY_METHOD_TIMEOUT),
            parse::parse_mempool_metadata,
            |error| {
                mempool_unavailable_or_fetch(error, || {
                    zaino_source::GetMempoolMetadataError::Unavailable
                })
            },
        )
        .await
    }
}

impl zaino_source::GetRawMempoolTransaction for ZebraRpcAdapter {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetRawMempoolTransactionError>> {
        // Verbosity 0: the bytes alone. The caller already knows this
        // transaction is in the mempool — it came from the listing — so the
        // location `getrawtransaction 1` would add is redundant.
        let params = vec![
            serde_json::Value::String(txid_to_display_hex(txid)),
            serde_json::Value::Number(0.into()),
        ];
        self.call_parsed_or_absent(
            "getrawtransaction",
            params,
            parse::parse_raw_transaction,
            || zaino_source::GetRawMempoolTransactionError::NotFound(txid),
        )
        .await
    }
}

impl zaino_source::GetMempoolSourceTip for ZebraRpcAdapter {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<std::convert::Infallible>> {
        // `getblockchaininfo` rather than the `GetChainTip` pair, because it
        // answers hash and height in one round trip. Reading them separately
        // would let a block land between the two calls and hand the mempool a
        // tip that never existed.
        let info: zaino_primitives::types::BlockchainInfo = self
            .call_parsed("getblockchaininfo", vec![], parse::parse_blockchain_info)
            .await?;

        Ok((info.best_block_hash, info.blocks))
    }
}

impl zaino_source::GetAddressBalance for ZebraRpcAdapter {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<
        zaino_primitives::types::AddressBalance,
        QueryError<zaino_source::GetAddressBalanceError>,
    > {
        self.call_parsed_or_invalid_address(
            "getaddressbalance",
            vec![addresses_param(addresses)],
            parse::parse_address_balance,
            zaino_source::GetAddressBalanceError::InvalidAddress,
        )
        .await
    }
}

impl zaino_source::GetAddressDeltas for ZebraRpcAdapter {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<
        Vec<zaino_primitives::types::AddressDelta>,
        QueryError<zaino_source::GetAddressDeltasError>,
    > {
        let params = vec![serde_json::json!({
            "addresses": addresses,
            "start": u32::from(start),
            "end": u32::from(end),
        })];
        self.call_parsed_or_invalid_address(
            "getaddressdeltas",
            params,
            parse::parse_address_deltas,
            zaino_source::GetAddressDeltasError::InvalidAddress,
        )
        .await
    }
}

impl zaino_source::GetAddressTxids for ZebraRpcAdapter {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<zaino_source::GetAddressTxidsError>> {
        let params = vec![serde_json::json!({
            "addresses": addresses,
            "start": u32::from(start),
            "end": u32::from(end),
        })];
        self.call_parsed_or_invalid_address(
            "getaddresstxids",
            params,
            parse::parse_txids,
            zaino_source::GetAddressTxidsError::InvalidAddress,
        )
        .await
    }
}

impl zaino_source::GetAddressUtxos for ZebraRpcAdapter {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, QueryError<zaino_source::GetAddressUtxosError>>
    {
        self.call_parsed_or_invalid_address(
            "getaddressutxos",
            vec![addresses_param(addresses)],
            parse::parse_address_utxos,
            zaino_source::GetAddressUtxosError::InvalidAddress,
        )
        .await
    }
}

impl zaino_source::GetTreestateByHash for ZebraRpcAdapter {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<zaino_source::GetTreestateByHashError>> {
        self.call_parsed_or_absent(
            "z_gettreestate",
            hash_param(hash),
            parse::parse_treestate,
            || zaino_source::GetTreestateByHashError::BlockNotFound(hash),
        )
        .await
    }
}

impl zaino_source::GetCommitmentTreeRoots for ZebraRpcAdapter {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<
        zaino_primitives::types::TreeRoots,
        QueryError<zaino_source::GetCommitmentTreeRootsError>,
    > {
        self.call_parsed_or_absent(
            "z_gettreestate",
            hash_param(block),
            parse::parse_tree_roots,
            || zaino_source::GetCommitmentTreeRootsError::BlockNotFound(block),
        )
        .await
    }
}

impl zaino_source::GetSubtreeRoots for ZebraRpcAdapter {
    async fn get_subtree_roots(
        &self,
        pool: zaino_primitives::types::ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<
        Vec<zaino_primitives::types::SubtreeRoot>,
        QueryError<zaino_source::GetSubtreeRootsError>,
    > {
        let mut params = vec![
            serde_json::Value::String(pool.to_string()),
            serde_json::Value::Number(start_index.into()),
        ];
        // Omit the limit rather than sending a sentinel: the validator's own
        // default applies when the argument is absent.
        if let Some(limit) = limit {
            params.push(serde_json::Value::Number(limit.into()));
        }
        self.call_parsed("z_getsubtreesbyindex", params, parse::parse_subtree_roots)
            .await
    }
}

impl zaino_source::GetSpentInfo for ZebraRpcAdapter {
    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_primitives::types::rpc::SpentInfo, QueryError<zaino_source::GetSpentInfoError>>
    {
        use zaino_source::GetSpentInfoError;

        let params = vec![serde_json::json!({
            "txid": txid_to_display_hex(outpoint.txid),
            "index": outpoint.index,
        })];

        let value = self
            .rpc
            .call("getspentinfo", params)
            .await
            .map_err(|error| {
                let error: FetchError = error.into();
                match spent_info_rejection(&error) {
                    Some(rejection) => QueryError::Domain(rejection),
                    None => QueryError::Fetch(error),
                }
            })?;

        // A `null` body would be the same fact by a different route. zcashd
        // does not send one, but reading it as "no spend on record" keeps the
        // two spellings from producing different answers.
        parse::parse_spent_info(&value)
            .map_err(|e| QueryError::Fetch(from_parse(e)))?
            .ok_or(QueryError::Domain(GetSpentInfoError::NotSpent))
    }
}

impl zaino_source::GetTxOut for ZebraRpcAdapter {
    async fn get_tx_out(
        &self,
        txid: TransactionId,
        index: zaino_primitives::types::OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, QueryError<zaino_source::GetTxOutError>>
    {
        let params = vec![
            serde_json::Value::String(txid_to_display_hex(txid)),
            serde_json::Value::Number(index.into()),
            serde_json::Value::Bool(include_mempool),
        ];
        self.call_parsed_optional("gettxout", params, parse::parse_tx_out)
            .await
    }
}

impl zaino_source::SendRawTransaction for ZebraRpcAdapter {
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<zaino_source::SendRawTransactionError>> {
        let params = vec![serde_json::Value::String(hex::encode(transaction))];
        // The one mutating call, and the one whose rejections are the point:
        // a client that submitted a bad transaction needs to know why.
        let value = match self.rpc.call("sendrawtransaction", params).await {
            Ok(value) => value,
            Err(error) => {
                let error: FetchError = error.into();
                return Err(match submission_rejection(&error) {
                    Some(rejection) => QueryError::Domain(rejection),
                    None => QueryError::Fetch(error),
                });
            }
        };
        parse::as_txid(&value).map_err(|e| QueryError::Fetch(from_parse(e)))
    }
}

impl zaino_source::GetNodeInfo for ZebraRpcAdapter {
    async fn get_node_info(
        &self,
    ) -> Result<zaino_primitives::types::rpc::NodeInfo, QueryError<zaino_source::GetNodeInfoError>>
    {
        self.call_parsed("getinfo", vec![], parse::parse_node_info)
            .await
    }
}

impl zaino_source::GetPeerInfo for ZebraRpcAdapter {
    async fn get_peer_info(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::PeerInfo>,
        QueryError<zaino_source::GetPeerInfoError>,
    > {
        self.call_parsed("getpeerinfo", vec![], parse::parse_peer_info)
            .await
    }
}

impl zaino_source::GetMiningInfo for ZebraRpcAdapter {
    async fn get_mining_info(
        &self,
    ) -> Result<
        zaino_primitives::types::rpc::MiningInfo,
        QueryError<zaino_source::GetMiningInfoError>,
    > {
        self.call_parsed("getmininginfo", vec![], parse::parse_mining_info)
            .await
    }
}

impl zaino_source::GetBlockSubsidy for ZebraRpcAdapter {
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<
        zaino_primitives::types::rpc::BlockSubsidy,
        QueryError<zaino_source::GetBlockSubsidyError>,
    > {
        let params = vec![serde_json::Value::Number(u32::from(height).into())];
        self.call_parsed_or_absent(
            "getblocksubsidy",
            params,
            parse::parse_block_subsidy,
            || zaino_source::GetBlockSubsidyError::HeightNotReached(height),
        )
        .await
    }
}

impl zaino_source::GetNetworkSolPs for ZebraRpcAdapter {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<zaino_source::GetNetworkSolPsError>> {
        // Both arguments are positional, so a height cannot be sent without a
        // window before it; the validator's own default window fills the gap.
        let mut params = Vec::new();
        if blocks.is_some() || height.is_some() {
            params.push(serde_json::Value::Number(blocks.unwrap_or(120).into()));
        }
        if let Some(height) = height {
            params.push(serde_json::Value::Number(u32::from(height).into()));
        }
        self.call_parsed("getnetworksolps", params, parse::as_u64)
            .await
    }
}

/// The RPC adapter owns no background work: it holds a connection pool that
/// drops with it, so there is nothing to release.
impl zaino_source::SourceLifecycle for ZebraRpcAdapter {}

/// Reaching the validator over request/response gives no push path, so this
/// adapter has no block-arrival signal to offer and inherits `None`. Consumers
/// pace themselves on their own timer.
impl zaino_source::SubscribeBlocks for ZebraRpcAdapter {}

impl zaino_source::GetTransaction for ZebraRpcAdapter {
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<zaino_source::TransactionResponse, QueryError<zaino_source::GetTransactionError>>
    {
        // Verbosity 1: the raw hex plus the height needed to place the
        // transaction. Verbosity 0 would return the bytes alone, leaving the
        // caller unable to tell a mined transaction from a mempool one.
        let params = vec![
            serde_json::Value::String(txid_to_display_hex(txid)),
            serde_json::Value::Number(1.into()),
        ];
        self.call_parsed_or_absent(
            "getrawtransaction",
            params,
            parse::parse_transaction,
            || zaino_source::GetTransactionError::NotFound(txid),
        )
        .await
    }
}

impl zaino_source::GetRawBlock for ZebraRpcAdapter {
    async fn get_raw_block(
        &self,
        height: Height,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockError>> {
        let params = vec![
            serde_json::Value::String(u32::from(height).to_string()),
            serde_json::Value::Number(0.into()),
        ];
        self.call_parsed_or_absent("getblock", params, parse::parse_raw_block, || {
            zaino_source::GetBlockError::HeightNotFound(height)
        })
        .await
    }
}

impl zaino_source::GetRawBlockByHash for ZebraRpcAdapter {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<zaino_source::GetBlockByHashError>> {
        let params = vec![
            serde_json::Value::String(hash_to_display_hex(hash)),
            serde_json::Value::Number(0.into()),
        ];
        self.call_parsed_or_absent("getblock", params, parse::parse_raw_block, || {
            zaino_source::GetBlockByHashError::NotFound(hash)
        })
        .await
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;
    use zaino_source::{GetBlockError, SendRawTransactionError};

    fn rpc(code: i64, message: &str) -> zaino_rpc::RpcError {
        zaino_rpc::RpcError::Rpc {
            code,
            message: message.to_string(),
        }
    }

    fn height() -> Height {
        Height::try_from(42u32).expect("a valid height")
    }

    /// The regression this classification exists for. `getblock` for a height
    /// above the tip answers `-8 Block not found`, which the sync loop asks for
    /// on every iteration. Reported as a fetch failure it exhausts the retry
    /// ladder and stops the indexer against a perfectly healthy validator.
    #[test]
    fn a_missing_block_is_an_answer_not_a_failure() {
        let classified: QueryError<GetBlockError> =
            absent_or_fetch(rpc(-8, "Block not found"), || {
                GetBlockError::HeightNotFound(height())
            });

        assert!(
            matches!(
                classified,
                QueryError::Domain(GetBlockError::HeightNotFound(_))
            ),
            "a missing block must be a domain answer, got {classified:?}"
        );
    }

    /// zebrad uses `-5` for the same answer on the hash-addressed methods.
    #[test]
    fn both_not_found_codes_are_recognised() {
        for code in NOT_FOUND_CODES {
            let classified: QueryError<GetBlockError> =
                absent_or_fetch(rpc(code, "not found"), || {
                    GetBlockError::HeightNotFound(height())
                });
            assert!(
                matches!(classified, QueryError::Domain(_)),
                "code {code} must read as absent"
            );
        }
    }

    /// Everything else stays a failure. A validator that is down, warming up,
    /// or erroring must not be reported as "the block does not exist" — that
    /// would have consumers treat an outage as an empty chain.
    #[test]
    fn other_codes_stay_fetch_failures() {
        for code in [-1, -3, -20, -22, -25, -28, -32_600] {
            let classified: QueryError<GetBlockError> =
                absent_or_fetch(rpc(code, "something else"), || {
                    GetBlockError::HeightNotFound(height())
                });
            assert!(
                matches!(classified, QueryError::Fetch(_)),
                "code {code} must stay a fetch failure"
            );
        }
    }

    /// Transport failures are never domain answers, whatever the port.
    #[test]
    fn transport_failures_stay_fetch_failures() {
        let classified: QueryError<GetBlockError> =
            absent_or_fetch(zaino_rpc::RpcError::Status(503), || {
                GetBlockError::HeightNotFound(height())
            });

        assert!(matches!(classified, QueryError::Fetch(_)));
    }

    /// On the address-keyed methods `-5` is the caller's address being
    /// rejected, and `-8` is a malformed request envelope — a Zaino bug, which
    /// must surface as a failure rather than as "that address has no history".
    #[test]
    fn only_minus_five_rejects_an_address() {
        use zaino_source::GetAddressBalanceError;

        let rejected: QueryError<GetAddressBalanceError> = invalid_address_or_fetch(
            rpc(-5, "invalid address"),
            GetAddressBalanceError::InvalidAddress,
        );
        assert!(matches!(
            rejected,
            QueryError::Domain(GetAddressBalanceError::InvalidAddress(_))
        ));

        let malformed: QueryError<GetAddressBalanceError> = invalid_address_or_fetch(
            rpc(-8, "start must be less than end"),
            GetAddressBalanceError::InvalidAddress,
        );
        assert!(
            matches!(malformed, QueryError::Fetch(_)),
            "a malformed request is Zaino's bug and must not read as a bad address"
        );
    }

    /// A rejected submission is an answer about the transaction. Reporting it
    /// as a transport failure loses the reason, which is the only useful part.
    #[test]
    fn a_rejected_submission_carries_its_reason() {
        let malformed = submission_rejection(&rpc(-22, "tx unparseable").into());
        assert!(matches!(
            malformed,
            Some(SendRawTransactionError::Malformed(_))
        ));

        for code in [-25, -26, -27] {
            assert!(
                matches!(
                    submission_rejection(&rpc(code, "rejected").into()),
                    Some(SendRawTransactionError::Rejected(_))
                ),
                "code {code} is the validator declining the transaction"
            );
        }

        assert!(
            submission_rejection(&rpc(-28, "warming up").into()).is_none(),
            "a warming-up validator has not considered the transaction at all"
        );
    }

    /// `getspentinfo` is zcashd-only, so a zebrad-backed deployment answers
    /// every call `-32601`. That must not be read as "unspent": one says the
    /// output has no spend on record, the other says this node cannot tell you
    /// either way, and collapsing them reports spent outputs as unspent.
    #[test]
    fn an_unsupported_method_is_not_an_unspent_output() {
        use zaino_source::GetSpentInfoError;

        assert_eq!(
            spent_info_rejection(&rpc(-5, "Unable to get spent info").into()),
            Some(GetSpentInfoError::NotSpent)
        );
        assert_eq!(
            spent_info_rejection(&rpc(-32601, "Method not found").into()),
            Some(GetSpentInfoError::Unsupported)
        );
    }

    /// Everything else is a failure to reach or be served by the node, and must
    /// stay retryable rather than becoming a claim about the outpoint.
    #[test]
    fn other_spent_info_codes_stay_fetch_failures() {
        for code in [-8, -28, -1, -32603] {
            assert!(
                spent_info_rejection(&rpc(code, "something else").into()).is_none(),
                "code {code} says nothing about whether the output was spent"
            );
        }

        assert!(spent_info_rejection(&FetchError::new(
            zaino_source::FailureMode::Timeout,
            "timed out"
        ))
        .is_none());
    }

    /// A validator that does not implement `getrawmempool` is answering about
    /// itself, not failing to answer. Reported as the port's domain error so a
    /// consumer stops asking, rather than as a fetch failure it would re-poll
    /// forever against a node that will never serve one.
    #[test]
    fn a_missing_mempool_method_is_the_ports_own_answer() {
        use zaino_source::GetMempoolTxidsError;

        let classified = mempool_unavailable_or_fetch(rpc(-32601, "Method not found"), || {
            GetMempoolTxidsError::Unavailable
        });
        assert!(matches!(
            classified,
            QueryError::Domain(GetMempoolTxidsError::Unavailable)
        ));
    }

    /// Everything else is a failure to reach or be served by the node. In
    /// particular an empty mempool is a successful empty list and never reaches
    /// here, and a warming-up validator (-28) is transient — calling either
    /// "unavailable" would tell a consumer to give up on a healthy node.
    #[test]
    fn other_mempool_codes_stay_fetch_failures() {
        use zaino_source::GetMempoolTxidsError;

        for code in [-5, -8, -28, -1, -32603] {
            let classified = mempool_unavailable_or_fetch(rpc(code, "something else"), || {
                GetMempoolTxidsError::Unavailable
            });
            assert!(
                matches!(classified, QueryError::Fetch(_)),
                "code {code} does not say this validator lacks a mempool"
            );
        }
    }
}
