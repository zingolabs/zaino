//! The new source stack, behind ChainIndex's scaffolding port.
//!
//! [`ZebraValidatorSource`] implements [`BlockchainSource`] — the temporary
//! port described in [`super::source`] — by delegating to
//! [`ZebraValidator`], the composite that routes each question to whichever
//! transport can answer it.
//!
//! # What this file is doing
//!
//! Translating *back*. The port's signatures still return `zebra-rpc` and
//! `zaino-fetch` wire shapes, because ChainIndex has not moved off them yet.
//! The composite answers in domain types. So roughly half the methods here
//! convert domain → wire, undoing part of what the adapters below did.
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

#[cfg(test)]
use hex::FromHex as _;
use zaino_source::{
    GetAddressBalance as _, GetAddressDeltas as _, GetAddressTxids as _, GetAddressUtxos as _,
    GetBestBlockHeight as _, GetBlockDeltas as _, GetBlockHeader as _, GetBlockSubsidy as _,
    GetBlockVerboseByHash as _, GetBlockchainInfo as _, GetChainTip as _, GetChainTips as _,
    GetCommitmentTreeRoots as _, GetDifficulty as _, GetMempoolTxids as _, GetMiningInfo as _,
    GetNetworkSolPs as _, GetNodeInfo as _, GetPeerInfo as _, GetRawBlock as _,
    GetRawBlockByHash as _, GetRawBlockHeader as _, GetSpentInfo as _, GetSubtreeRoots as _,
    GetTransaction as _, GetTreestate as _, GetTreestateByHash as _, GetTxOut as _, QueryError,
    SendRawTransaction as _, SourceLifecycle as _, SubscribeBlocks as _,
};
use zaino_source_zebra::ZebraValidator;
use zebra_chain::serialization::BytesInDisplayOrder as _;
use zebra_rpc::methods::ValidateAddresses as _;
use zebra_state::HashOrHeight;

use super::source::{BlockchainSource, BlockchainSourceError, BlockchainSourceResult};

/// ChainIndex's validator source, backed by the `zaino-source` stack.
#[derive(Clone)]
pub struct ZebraValidatorSource {
    /// Shared because the port requires `Clone` and the composite owns
    /// connections and a database handle that must not be duplicated.
    validator: Arc<ZebraValidator>,

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

impl ZebraValidatorSource {
    /// Wrap a composite validator as ChainIndex's source.
    pub fn new(
        validator: ZebraValidator,
        network: zebra_chain::parameters::Network,
        chain_tip_change: Option<zebra_state::ChainTipChange>,
    ) -> Self {
        Self {
            validator: Arc::new(validator),
            network,
            chain_tip_change,
        }
    }
}

impl std::fmt::Debug for ZebraValidatorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZebraValidatorSource")
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
fn err<E>(error: QueryError<E>) -> BlockchainSourceError
where
    E: std::fmt::Debug + std::fmt::Display,
{
    match error {
        QueryError::Domain(e) => {
            BlockchainSourceError::Unrecoverable(format!("validator rejected the query: {e}"))
        }
        QueryError::Fetch(e) => {
            BlockchainSourceError::Unrecoverable(format!("validator unreachable: {e}"))
        }
    }
}

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

/// Render a 32-byte identifier in RPC display order.
///
/// Hashes and txids are byte-reversed on this interface; the domain types hold
/// them in internal order, so the reversal happens on the way out.
fn display_hex(mut bytes: [u8; 32]) -> String {
    bytes.reverse();
    hex::encode(bytes)
}

fn invalid(message: String) -> BlockchainSourceError {
    BlockchainSourceError::Unrecoverable(message)
}

impl ZebraValidatorSource {
    /// Identify the block at a height, for the chaininfo delta form.
    async fn block_info(
        &self,
        height: u32,
    ) -> Result<zaino_fetch::jsonrpsee::response::address_deltas::BlockInfo, BlockchainSourceError>
    {
        use zaino_source::GetRawBlock as _;
        let bytes = self
            .validator
            .get_raw_block(domain_height(height)?)
            .await
            .map_err(err)?;
        let block = block_from_bytes(bytes)?;
        Ok(
            zaino_fetch::jsonrpsee::response::address_deltas::BlockInfo::new(
                hex::encode(block.hash().bytes_in_display_order()),
                height,
            ),
        )
    }
}

/// Zatoshis rendered as the ZEC decimal this interface expects.
fn zats_to_zec(amount: zaino_primitives::types::Zatoshis) -> f64 {
    u64::from(amount) as f64 / 100_000_000.0
}

/// Parse a txid written in RPC display order.
fn parse_display_txid(
    hex_str: &str,
) -> Result<zaino_primitives::types::TransactionHash, BlockchainSourceError> {
    let bytes = hex::decode(hex_str).map_err(|e| invalid(format!("txid is not hex: {e}")))?;
    let mut internal: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("txid is not 32 bytes".to_string()))?;
    internal.reverse();
    Ok(zaino_primitives::types::TransactionHash::from(internal))
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
        "deferred" => GetBlockchainInfoBalance::deferred(value, delta),
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
            "deferred" => 4,
            "ironwood" => 5,
            other => return Err(invalid(format!("unknown value pool `{other}`"))),
        };
        slots[slot] = built;
    }
    Ok(slots)
}

impl BlockchainSource for ZebraValidatorSource {
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

    // ***** Mempool *****

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let txids = self.validator.get_mempool_txids().await.map_err(err)?;
        Ok(Some(
            txids
                .into_iter()
                .map(|txid| zebra_chain::transaction::Hash::from(<[u8; 32]>::from(txid)))
                .collect(),
        ))
    }

    // ***** Transparent addresses *****

    async fn get_address_balance(
        &self,
        address_strings: zebra_rpc::client::GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<zebra_rpc::methods::AddressBalance> {
        let balance = self
            .validator
            .get_address_balance(address_strings_to_vec(&address_strings)?)
            .await
            .map_err(err)?;

        Ok(zebra_rpc::methods::AddressBalance::new(
            u64::from(balance.balance),
            u64::from(balance.received),
        ))
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
    ) -> BlockchainSourceResult<Vec<zebra_rpc::methods::GetAddressUtxos>> {
        let utxos = self
            .validator
            .get_address_utxos(address_strings_to_vec(&address_strings)?)
            .await
            .map_err(err)?;

        utxos
            .into_iter()
            .map(|utxo| {
                Ok(zebra_rpc::methods::GetAddressUtxos::new(
                    utxo.address
                        .as_str()
                        .parse()
                        .map_err(|e| invalid(format!("utxo address: {e}")))?,
                    zebra_chain::transaction::Hash::from(<[u8; 32]>::from(utxo.txid)),
                    zebra_chain::transparent::OutputIndex::from_index(utxo.output_index),
                    zebra_chain::transparent::Script::new(&Vec::<u8>::from(utxo.script)),
                    u64::from(utxo.satoshis),
                    zebra_chain::block::Height(utxo.height.into()),
                ))
            })
            .collect()
    }

    async fn get_address_deltas(
        &self,
        params: zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasParams,
    ) -> BlockchainSourceResult<
        zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasResponse,
    > {
        use zaino_fetch::jsonrpsee::response::address_deltas::{
            AddressDelta, GetAddressDeltasParams, GetAddressDeltasResponse,
        };

        let (addresses, start, end, chain_info) = match &params {
            GetAddressDeltasParams::Filtered {
                addresses,
                start,
                end,
                chain_info,
            } => (addresses.clone(), *start, *end, *chain_info),
            // The single-address form carries no range, which the interface
            // reads as "the whole chain".
            GetAddressDeltasParams::Address(address) => (vec![address.clone()], 0, 0, false),
        };

        let deltas = self
            .validator
            .get_address_deltas(addresses, domain_height(start)?, domain_height(end)?)
            .await
            .map_err(err)?;

        let deltas: Vec<AddressDelta> = deltas
            .into_iter()
            .map(|delta| {
                AddressDelta::new(
                    i64::from(delta.satoshis),
                    display_hex(<[u8; 32]>::from(delta.txid)),
                    delta.index,
                    u32::from(delta.height),
                    String::from(delta.address),
                    // The port does not carry the transaction's position within
                    // its block; the interface marks it optional, so it is
                    // reported absent rather than guessed.
                    None,
                )
            })
            .collect();

        if !chain_info {
            return Ok(GetAddressDeltasResponse::Simple(deltas));
        }

        // The chaininfo form additionally names the range's bounding blocks.
        Ok(GetAddressDeltasResponse::WithChainInfo {
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

        let domain_txid = zaino_primitives::types::TransactionHash::from(txid.0);

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
        verbose: bool,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader>
    {
        use zaino_fetch::jsonrpsee::response::block_header::{GetBlockHeader, VerboseBlockHeader};

        let block_hash = zaino_primitives::types::BlockHash::from(parse_display_hash32(&hash)?);

        // Verbosity is a request parameter, so the port splits it into two
        // questions rather than returning something the caller must match on.
        if !verbose {
            let raw = self
                .validator
                .get_raw_block_header(block_hash)
                .await
                .map_err(err)?;
            return Ok(GetBlockHeader::Compact(hex::encode(raw)));
        }

        let header = self
            .validator
            .get_block_header(block_hash)
            .await
            .map_err(err)?;

        Ok(GetBlockHeader::Verbose(VerboseBlockHeader {
            hash: zebra_chain::block::Hash(header.hash.into()),
            confirmations: header.confirmations,
            height: header.height.into(),
            version: header.version,
            merkle_root: zebra_chain::block::merkle::Root(header.merkle_root.into()),
            block_commitments: header.block_commitments.map(<[u8; 32]>::from),
            final_sapling_root: header.final_sapling_root.map(<[u8; 32]>::from),
            time: i64::from(header.time),
            nonce: hex::encode(header.nonce),
            solution: hex::encode(header.solution),
            bits: format!("{:08x}", header.bits),
            difficulty: header.difficulty,
            chainwork: header
                .chainwork
                .map(|work| hex::encode(<[u8; 32]>::from(work))),
            previous_block_hash: header
                .previous_block_hash
                .map(|h| display_hex(<[u8; 32]>::from(h))),
            next_block_hash: header
                .next_block_hash
                .map(|h| display_hex(<[u8; 32]>::from(h))),
        }))
    }

    async fn get_block_deltas(
        &self,
        hash: String,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::block_deltas::BlockDeltas> {
        use zaino_fetch::jsonrpsee::response::block_deltas::{
            BlockDelta, BlockDeltas, InputDelta, OutputDelta,
        };

        let deltas = self
            .validator
            .get_block_deltas(zaino_primitives::types::BlockHash::from(
                parse_display_hash32(&hash)?,
            ))
            .await
            .map_err(err)?;

        Ok(BlockDeltas {
            hash: display_hex(<[u8; 32]>::from(deltas.hash)),
            confirmations: deltas.confirmations,
            size: deltas.size as i64,
            height: deltas.height.into(),
            version: deltas.version,
            merkle_root: display_hex(<[u8; 32]>::from(deltas.merkle_root)),
            time: i64::from(deltas.time),
            median_time: i64::from(deltas.median_time),
            nonce: hex::encode(deltas.nonce),
            bits: format!("{:08x}", deltas.bits),
            difficulty: deltas.difficulty,
            previous_block_hash: deltas
                .previous_block_hash
                .map(|h| display_hex(<[u8; 32]>::from(h))),
            next_block_hash: deltas
                .next_block_hash
                .map(|h| display_hex(<[u8; 32]>::from(h))),
            deltas: deltas
                .deltas
                .into_iter()
                .map(|delta| {
                    Ok(BlockDelta {
                        txid: display_hex(<[u8; 32]>::from(delta.txid)),
                        index: delta.index,
                        inputs: delta
                            .inputs
                            .into_iter()
                            .map(|input| {
                                Ok(InputDelta {
                                    address: String::from(input.address),
                                    satoshis: amount(i64::from(input.satoshis))?,
                                    index: input.index,
                                    prevtxid: display_hex(<[u8; 32]>::from(input.prev_txid)),
                                    prevout: input.prev_output,
                                })
                            })
                            .collect::<Result<Vec<_>, BlockchainSourceError>>()?,
                        outputs: delta
                            .outputs
                            .into_iter()
                            .map(|output| {
                                Ok(OutputDelta {
                                    address: String::from(output.address),
                                    satoshis: amount(
                                        i64::try_from(u64::from(output.satoshis)).map_err(
                                            |_| invalid("output value out of range".to_string()),
                                        )?,
                                    )?,
                                    index: output.index,
                                })
                            })
                            .collect::<Result<Vec<_>, BlockchainSourceError>>()?,
                    })
                })
                .collect::<Result<Vec<_>, BlockchainSourceError>>()?,
        })
    }

    async fn get_blockchain_info(
        &self,
    ) -> BlockchainSourceResult<zebra_rpc::methods::GetBlockchainInfoResponse> {
        use zebra_rpc::methods::{
            ConsensusBranchIdHex, NetworkUpgradeInfo, NetworkUpgradeStatus, TipConsensusBranch,
        };

        let info = self.validator.get_blockchain_info().await.map_err(err)?;

        let upgrades: indexmap::IndexMap<_, _> = info
            .upgrades
            .into_iter()
            .map(|upgrade| {
                let branch =
                    zebra_chain::parameters::ConsensusBranchId::from(u32::from(upgrade.branch_id));
                let status = match upgrade.status {
                    zaino_primitives::types::NetworkUpgradeStatus::Active => {
                        NetworkUpgradeStatus::Active
                    }
                    zaino_primitives::types::NetworkUpgradeStatus::Pending => {
                        NetworkUpgradeStatus::Pending
                    }
                    zaino_primitives::types::NetworkUpgradeStatus::Disabled => {
                        NetworkUpgradeStatus::Disabled
                    }
                };
                // The interface names upgrades by their enum, the port by their
                // consensus branch id — the protocol-defined identity. There is
                // no direct conversion, so the network's own activation list is
                // the lookup. An upgrade this build does not know about is
                // rejected rather than guessed: Zaino adopts this schedule as
                // its activation heights, and a wrong entry would put it on
                // different consensus rules from its validator.
                let named = self
                    .network
                    .full_activation_list()
                    .into_iter()
                    .find_map(|(_height, upgrade)| {
                        (upgrade.branch_id() == Some(branch)).then_some(upgrade)
                    })
                    .ok_or_else(|| {
                        invalid(format!(
                            "validator reported consensus branch {branch:?}, \
                             which this build does not recognise"
                        ))
                    })?;
                Ok((
                    ConsensusBranchIdHex::new(branch.into()),
                    NetworkUpgradeInfo::from_parts(
                        named,
                        zebra_chain::block::Height(upgrade.activation_height.into()),
                        status,
                    ),
                ))
            })
            .collect::<Result<_, BlockchainSourceError>>()?;

        Ok(zebra_rpc::methods::GetBlockchainInfoResponse::new(
            info.chain,
            zebra_chain::block::Height(info.blocks.into()),
            zebra_chain::block::Hash(info.best_block_hash.into()),
            zebra_chain::block::Height(info.estimated_height.into()),
            pool_balance(Some(&info.chain_supply))?,
            value_pool_array(&info.value_pools)?,
            upgrades,
            TipConsensusBranch::from_parts(
                ConsensusBranchIdHex::new(u32::from(info.consensus.chain_tip)).inner(),
                ConsensusBranchIdHex::new(u32::from(info.consensus.next_block)).inner(),
            ),
            zebra_chain::block::Height(info.headers.into()),
            info.difficulty,
            info.verification_progress,
            // The interface types cumulative work as a 64-bit integer, which
            // cannot hold a real mainnet value. The port reports `None` where
            // the validator does not track it; zero is what this field has
            // always carried in that case.
            0,
            info.pruned,
            info.size_on_disk,
            info.commitments,
        ))
    }

    // ***** Shielded trees *****

    async fn get_treestate(
        &self,
        id: super::types::BlockHash,
    ) -> BlockchainSourceResult<super::source::TreestateBytes> {
        use super::source::PoolTreestate;

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

        // A pool is reported only when it has a tree at this block. Its root is
        // attached when the validator also reported one — Zebra does not, so
        // the field is genuinely absent rather than zeroed.
        let pool = |state: Option<Vec<u8>>, root: Option<zaino_primitives::types::TreeRootInfo>| {
            state.map(|final_state| PoolTreestate {
                final_root: root.map(|info| {
                    // The interface writes roots in display order, and the
                    // domain holds them internally, so this reverses on the
                    // way out — as `display_hex` does for identifiers.
                    let mut bytes = <[u8; 32]>::from(info.root);
                    bytes.reverse();
                    bytes.to_vec()
                }),
                final_state,
            })
        };

        Ok((
            pool(trees.sapling, roots.sapling),
            pool(trees.orchard, roots.orchard),
            pool(trees.ironwood, roots.ironwood),
        ))
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> BlockchainSourceResult<zebra_rpc::client::GetTreestateResponse> {
        use zebra_rpc::client::{Commitments, Treestate};

        // The scaffolding takes an unparsed identifier; the port takes one or
        // the other, so it is resolved here.
        let trees = match hash_or_height.parse::<u32>() {
            Ok(height) => self
                .validator
                .get_treestate(domain_height(height)?)
                .await
                .map_err(err)?,
            Err(_) => {
                let hash = zaino_primitives::types::BlockHash::from(parse_display_hash32(
                    &hash_or_height,
                )?);
                self.validator
                    .get_treestate_by_hash(hash)
                    .await
                    .map_err(err)?
            }
        };

        // `finalRoot` is left absent, matching Zebra, whose own type documents
        // the field as unused. The trees themselves are the answer.
        let pool = |state: Option<Vec<u8>>| Treestate::new(Commitments::new(None, state));

        Ok(zebra_rpc::client::GetTreestateResponse::new(
            zebra_chain::block::Hash(trees.block_hash.into()),
            zebra_chain::block::Height(trees.height.into()),
            trees.time,
            // Sprout is never served: Zaino does not index it, and reporting an
            // empty tree would claim knowledge it does not have.
            None,
            pool(trees.sapling),
            pool(trees.orchard),
            Some(pool(trees.ironwood)),
        ))
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

    async fn get_info(&self) -> BlockchainSourceResult<zebra_rpc::methods::GetInfo> {
        let info = self.validator.get_node_info().await.map_err(err)?;

        Ok(zebra_rpc::methods::GetInfo::new(
            info.version,
            info.build,
            info.subversion,
            info.protocol_version,
            info.blocks.into(),
            info.connections as usize,
            info.proxy,
            info.difficulty,
            info.testnet,
            zats_to_zec(info.pay_tx_fee),
            zats_to_zec(info.relay_fee),
            // The port normalises "healthy" to absence; this interface signals
            // it with a sentinel string, so the sentinel is restored here.
            info.errors.unwrap_or_else(|| "no errors".to_string()),
            info.errors_timestamp.unwrap_or_default(),
        ))
    }

    async fn get_peer_info(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::peer_info::GetPeerInfo> {
        use zaino_fetch::jsonrpsee::response::peer_info::{GetPeerInfo, ZebradPeerInfo};

        let peers = self.validator.get_peer_info().await.map_err(err)?;

        // Always the zebrad shape: the port models the two fields every
        // validator reports, which is exactly that variant.
        Ok(GetPeerInfo::Zebrad(
            peers
                .into_iter()
                .map(|peer| ZebradPeerInfo {
                    addr: peer.addr,
                    inbound: peer.inbound,
                })
                .collect(),
        ))
    }

    async fn get_chain_tips(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::chain_tips::GetChainTipsResponse>
    {
        use zaino_fetch::jsonrpsee::response::chain_tips::{ChainTip, ChainTipStatus};
        use zaino_primitives::types::rpc::ChainTipStatus as DomainStatus;

        let tips = self.validator.get_chain_tips().await.map_err(err)?;

        Ok(tips
            .into_iter()
            .map(|tip| {
                let status = match tip.status {
                    DomainStatus::Active => ChainTipStatus::Active,
                    DomainStatus::ValidFork => ChainTipStatus::ValidFork,
                    DomainStatus::ValidHeaders => ChainTipStatus::ValidHeaders,
                    DomainStatus::HeadersOnly => ChainTipStatus::HeadersOnly,
                    DomainStatus::Invalid => ChainTipStatus::Invalid,
                    DomainStatus::Unknown => ChainTipStatus::Unknown,
                };
                ChainTip::new(
                    tip.height.into(),
                    display_hex(<[u8; 32]>::from(tip.hash)),
                    tip.branch_len,
                    status,
                )
            })
            .collect())
    }

    async fn get_mining_info(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::mining_info::GetMiningInfoWire>
    {
        use zaino_fetch::jsonrpsee::response::mining_info::MiningInfo;

        let info = self.validator.get_mining_info().await.map_err(err)?;

        // Two hops: the wire type's fields are private, so it is reached
        // through `zaino-fetch`'s own internal representation.
        Ok(MiningInfo {
            tip_height: info.tip_height.into(),
            current_block_size: info.current_block_size,
            current_block_tx: info.current_block_tx,
            network_solution_rate: info.network_solution_rate,
            network_hash_rate: info.network_hash_rate,
            chain: info.chain,
            testnet: info.testnet,
            difficulty: info.difficulty,
            errors: info.errors,
            // The port drops fields it does not model rather than carrying
            // opaque values; there is nothing to restore here.
            extras: Default::default(),
        }
        .into())
    }

    async fn get_block_subsidy(
        &self,
        height: u32,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::block_subsidy::GetBlockSubsidy>
    {
        use zaino_fetch::jsonrpsee::response::block_subsidy::{
            BlockSubsidy, FundingStream, GetBlockSubsidy, LockBoxStream,
        };
        use zaino_fetch::jsonrpsee::response::common::amount::{Zatoshis, ZecAmount};

        let subsidy = self
            .validator
            .get_block_subsidy(domain_height(height)?)
            .await
            .map_err(err)?;

        let zec = |amount: zaino_primitives::types::Zatoshis| ZecAmount::from_zats(amount.into());

        Ok(GetBlockSubsidy::Known(BlockSubsidy {
            miner: zec(subsidy.miner),
            founders: zec(subsidy.founders),
            funding_streams_total: zec(subsidy.funding_streams_total),
            lockbox_total: zec(subsidy.lockbox_total),
            total_block_subsidy: zec(subsidy.total_block_subsidy),
            funding_streams: subsidy
                .funding_streams
                .into_iter()
                .map(|stream| FundingStream {
                    recipient: stream.recipient,
                    specification: stream.specification,
                    value: zec(stream.value),
                    value_zat: Zatoshis(stream.value.into()),
                    address: stream.address,
                })
                .collect(),
            lockbox_streams: subsidy
                .lockbox_streams
                .into_iter()
                .map(|stream| LockBoxStream {
                    recipient: stream.recipient,
                    specification: stream.specification,
                    value: zec(stream.value),
                    value_zat: Zatoshis(stream.value.into()),
                })
                .collect(),
        }))
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetNetworkSolPsResponse> {
        // Negative values are how this interface spells "use your default", so
        // they become absence rather than an error.
        let blocks = blocks.and_then(|b| u32::try_from(b).ok());
        let height = match height.and_then(|h| u32::try_from(h).ok()) {
            Some(h) => Some(domain_height(h)?),
            None => None,
        };

        let rate = self
            .validator
            .get_network_sol_ps(blocks, height)
            .await
            .map_err(err)?;

        Ok(zaino_fetch::jsonrpsee::response::GetNetworkSolPsResponse(
            rate,
        ))
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> BlockchainSourceResult<zebra_rpc::methods::SentTransactionHash> {
        let bytes = hex::decode(&raw_transaction_hex)
            .map_err(|e| invalid(format!("transaction is not hex: {e}")))?;

        let txid = self
            .validator
            .send_raw_transaction(bytes)
            .await
            .map_err(err)?;

        Ok(zebra_rpc::methods::SentTransactionHash::new(
            zebra_chain::transaction::Hash::from(<[u8; 32]>::from(txid)),
        ))
    }

    async fn get_spent_info(
        &self,
        request: zaino_fetch::jsonrpsee::response::GetSpentInfoRequest,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetSpentInfoResponse> {
        use zaino_primitives::types::rpc::SpentOutpoint;

        let outpoint = SpentOutpoint {
            txid: parse_display_txid(&request.txid)?,
            index: request.index,
        };

        let spent = self
            .validator
            .get_spent_info(outpoint)
            .await
            .map_err(err)?
            .ok_or_else(|| invalid("output is unspent or unknown".to_string()))?;

        Ok(zaino_fetch::jsonrpsee::response::GetSpentInfoResponse {
            txid: display_hex(<[u8; 32]>::from(spent.txid)),
            index: spent.index,
            height: spent.height.into(),
        })
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetTxOutResponse> {
        let out = self
            .validator
            .get_tx_out(
                parse_display_txid(&txid)?,
                n,
                include_mempool.unwrap_or(true),
            )
            .await
            .map_err(err)?;

        // This response is untyped on the wire, so the JSON is built here from
        // the modelled value rather than forwarded. A spent or unknown outpoint
        // is JSON `null`, which is the interface's real answer to "is this
        // unspent?" rather than an error.
        let Some(out) = out else {
            return Ok(zaino_fetch::jsonrpsee::response::GetTxOutResponse(None));
        };

        let mut script = serde_json::Map::new();
        script.insert(
            "hex".to_string(),
            serde_json::Value::String(hex::encode(Vec::<u8>::from(out.script_pub_key.script))),
        );
        if let Some(asm) = out.script_pub_key.asm {
            script.insert("asm".to_string(), serde_json::Value::String(asm));
        }
        if let Some(kind) = out.script_pub_key.script_type {
            script.insert("type".to_string(), serde_json::Value::String(kind));
        }
        if let Some(req_sigs) = out.script_pub_key.required_signatures {
            script.insert("reqSigs".to_string(), serde_json::Value::from(req_sigs));
        }
        if !out.script_pub_key.addresses.is_empty() {
            script.insert(
                "addresses".to_string(),
                serde_json::Value::Array(
                    out.script_pub_key
                        .addresses
                        .into_iter()
                        .map(|a| serde_json::Value::String(String::from(a)))
                        .collect(),
                ),
            );
        }

        Ok(zaino_fetch::jsonrpsee::response::GetTxOutResponse(Some(
            serde_json::json!({
                "bestblock": display_hex(<[u8; 32]>::from(out.best_block)),
                "confirmations": out.confirmations,
                "value": zats_to_zec(out.value),
                "valueZat": u64::from(out.value),
                "scriptPubKey": serde_json::Value::Object(script),
                "coinbase": out.coinbase,
            }),
        )))
    }

    // ***** Lifecycle *****

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

    /// Identifiers are byte-reversed on this interface but held in internal
    /// order by the domain types, so the reversal belongs on the way out — and
    /// must be exactly one reversal, not zero and not two.
    #[test]
    fn display_hex_reverses_exactly_once() {
        let rendered = display_hex(ASYMMETRIC);

        let mut expected = ASYMMETRIC;
        expected.reverse();
        assert_eq!(rendered, hex::encode(expected));

        // Round-tripping back through zebra's own display-order parser must
        // recover the bytes we started with.
        let parsed = zebra_chain::block::Hash::from_hex(&rendered).expect("valid hash hex");
        assert_eq!(parsed.0, ASYMMETRIC, "one reversal too many or too few");
    }

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

    /// Amounts cross the port as exact zatoshis and this interface wants ZEC.
    /// The conversion must be exact for values the protocol allows.
    #[test]
    fn zatoshi_to_zec_conversion_is_exact() {
        let one_zec = zaino_primitives::types::Zatoshis::new(100_000_000).expect("valid");
        assert_eq!(zats_to_zec(one_zec), 1.0);

        let dust = zaino_primitives::types::Zatoshis::new(1).expect("valid");
        assert_eq!(zats_to_zec(dust), 0.000_000_01);

        assert_eq!(zats_to_zec(zaino_primitives::types::Zatoshis::ZERO), 0.0);
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
