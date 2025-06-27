//! Holds the Indexer trait containing the zcash RPC definitions served by zaino
//! and generic wrapper structs for the various backend options available.

use async_trait::async_trait;

use tokio::{sync::mpsc, time::timeout};
use tracing::warn;
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
        SendResponse, ShieldedProtocol, SubtreeRoot, TransparentAddressBlockFilter, TreeState,
        TxFilter,
    },
};
use zebra_chain::{block::Height, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{
    trees::{GetSubtrees, GetTreestate},
    AddressBalance, AddressStrings, GetAddressTxIdsRequest, GetAddressUtxos, GetBlock,
    GetBlockChainInfo, GetInfo, GetRawTransaction, SentTransactionHash,
};

use crate::{
    status::StatusType,
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        SubtreeRootReplyStream, UtxoReplyStream,
    },
};

/// Wrapper Struct for a ZainoState chain-fetch service (StateService, FetchService)
///
/// The future plan is to also add a TonicService and DarksideService to this to enable
/// wallets to use a single unified chain fetch service.
#[derive(Clone)]
pub struct IndexerService<Service: ZcashService> {
    /// Underlying Service.
    service: Service,
}

impl<Service> IndexerService<Service>
where
    Service: ZcashService,
{
    /// Creates a new `IndexerService` using the provided `config`.
    pub async fn spawn(
        config: Service::Config,
    ) -> Result<Self, <Service::Subscriber as ZcashIndexer>::Error> {
        Ok(IndexerService {
            service: Service::spawn(config)
                .await
                .map_err(Into::<tonic::Status>::into)?,
        })
    }

    /// Returns a reference to the inner service.
    pub fn inner_ref(&self) -> &Service {
        &self.service
    }

    /// Consumes the `IndexerService` and returns the inner service.
    pub fn inner(self) -> Service {
        self.service
    }
}

/// Zcash Service functionality.
#[async_trait]
pub trait ZcashService: Sized {
    /// A subscriber to the service, used to fetch chain data.
    type Subscriber: Clone + ZcashIndexer + LightWalletIndexer;

    /// Service Config.
    type Config: Clone;

    /// Spawns a [`Service`].
    async fn spawn(config: Self::Config)
        -> Result<Self, <Self::Subscriber as ZcashIndexer>::Error>;

    /// Returns a [`ServiceSubscriber`].
    fn get_subscriber(&self) -> IndexerSubscriber<Self::Subscriber>;

    /// Fetches the current status
    async fn status(&self) -> StatusType;

    /// Shuts down the StateService.
    fn close(&mut self);
}

/// Wrapper Struct for a ZainoState chain-fetch service subscriber (StateServiceSubscriber, FetchServiceSubscriber)
///
/// The future plan is to also add a TonicServiceSubscriber and DarksideServiceSubscriber to this to enable wallets to use a single unified chain fetch service.
#[derive(Clone)]
pub struct IndexerSubscriber<Subscriber: Clone + ZcashIndexer + LightWalletIndexer + Send + Sync> {
    /// Underlying Service Subscriber.
    subscriber: Subscriber,
}

impl<Subscriber> IndexerSubscriber<Subscriber>
where
    Subscriber: Clone + ZcashIndexer + LightWalletIndexer,
{
    /// Creates a new [`IndexerSubscriber`].
    pub fn new(subscriber: Subscriber) -> Self {
        IndexerSubscriber { subscriber }
    }

    /// Returns a reference to the inner service.
    pub fn inner_ref(&self) -> &Subscriber {
        &self.subscriber
    }

    /// Returns a clone of the inner service.
    pub fn inner_clone(&self) -> Subscriber {
        self.subscriber.clone()
    }

    /// Consumes the `IndexerService` and returns the inner service.
    pub fn inner(self) -> Subscriber {
        self.subscriber
    }
}

/// Zcash RPC method signatures.
///
/// Doc comments taken from Zebra for consistency.
#[async_trait]
pub trait ZcashIndexer: Send + Sync + 'static {
    /// Uses underlying error type of implementer.
    type Error: std::error::Error
        + From<tonic::Status>
        + Into<tonic::Status>
        + Send
        + Sync
        + 'static;

    /// Returns software information from the RPC server, as a [`GetInfo`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    ///
    /// # Notes
    ///
    /// [The zcashd reference](https://zcash.github.io/rpc/getinfo.html) might not show some fields
    /// in Zebra's [`GetInfo`]. Zebra uses the field names and formats from the
    /// [zcashd code](https://github.com/zcash/zcash/blob/v4.6.0-1/src/rpc/misc.cpp#L86-L87).
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetInfo`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L91-L95)
    async fn get_info(&self) -> Result<GetInfo, Self::Error>;

    /// Returns blockchain state information, as a [`GetBlockChainInfo`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetBlockChainInfo`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L72-L89)
    async fn get_blockchain_info(&self) -> Result<GetBlockChainInfo, Self::Error>;

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    async fn get_difficulty(&self) -> Result<f64, Self::Error>;

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`] instance.
    ///
    /// zcashd reference: [`getaddressbalance`](https://zcash.github.io/rpc/getaddressbalance.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `address_strings`: (object, example={"addresses": ["tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ"]}) A JSON map with a single entry
    ///     - `addresses`: (array of strings) A list of base-58 encoded addresses.
    ///
    /// # Notes
    ///
    /// zcashd also accepts a single string parameter instead of an array of strings, but Zebra
    /// doesn't because lightwalletd always calls this RPC with an array of addresses.
    ///
    /// zcashd also returns the total amount of Zatoshis received by the addresses, but Zebra
    /// doesn't because lightwalletd doesn't use that information.
    ///
    /// The RPC documentation says that the returned object has a string `balance` field, but
    /// zcashd actually [returns an
    /// integer](https://github.com/zcash/lightwalletd/blob/bdaac63f3ee0dbef62bde04f6817a9f90d483b00/common/common.go#L128-L130).
    async fn z_get_address_balance(
        &self,
        address_strings: AddressStrings,
    ) -> Result<AddressBalance, Self::Error>;

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    /// Returns the [`SentTransactionHash`] for the transaction, as a JSON string.
    ///
    /// zcashd reference: [`sendrawtransaction`](https://zcash.github.io/rpc/sendrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `raw_transaction_hex`: (string, required, example="signedhex") The hex-encoded raw transaction bytes.
    ///
    /// # Notes
    ///
    /// zcashd accepts an optional `allowhighfees` parameter. Zebra doesn't support this parameter,
    /// because lightwalletd doesn't use it.
    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, Self::Error>;

    /// Returns the requested block by hash or height, as a [`GetBlock`] JSON string.
    /// If the block is not in Zebra's state, returns
    /// [error code `-8`.](https://github.com/zcash/zcash/issues/5758) if a height was
    /// passed or -5 if a hash was passed.
    ///
    /// zcashd reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbosity`: (number, optional, default=1, example=1) 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data.
    ///
    /// # Notes
    ///
    /// Zebra previously partially supported verbosity=1 by returning only the
    /// fields required by lightwalletd ([`lightwalletd` only reads the `tx`
    /// field of the result](https://github.com/zcash/lightwalletd/blob/dfac02093d85fb31fb9a8475b884dd6abca966c7/common/common.go#L152)).
    /// That verbosity level was migrated to "3"; so while lightwalletd will
    /// still work by using verbosity=1, it will sync faster if it is changed to
    /// use verbosity=3.
    ///
    /// The undocumented `chainwork` field is not returned.
    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error>;

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    async fn get_block_count(&self) -> Result<Height, Self::Error>;

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error>;

    /// Returns information about the given block's Sapling & Orchard tree state.
    ///
    /// zcashd reference: [`z_gettreestate`](https://zcash.github.io/rpc/z_gettreestate.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash | height`: (string, required, example="00000000febc373a1da2bd9f887b105ad79ddc26ac26c2b28652d64e5207c5b5") The block hash or height.
    ///
    /// # Notes
    ///
    /// The zcashd doc reference above says that the parameter "`height` can be
    /// negative where -1 is the last known valid block". On the other hand,
    /// `lightwalletd` only uses positive heights, so Zebra does not support
    /// negative heights.
    async fn z_get_treestate(&self, hash_or_height: String) -> Result<GetTreestate, Self::Error>;

    /// Returns information about a range of Sapling or Orchard subtrees.
    ///
    /// zcashd reference: [`z_getsubtreesbyindex`](https://zcash.github.io/rpc/z_getsubtreesbyindex.html) - TODO: fix link
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `pool`: (string, required) The pool from which subtrees should be returned. Either "sapling" or "orchard".
    /// - `start_index`: (number, required) The index of the first 2^16-leaf subtree to return.
    /// - `limit`: (number, optional) The maximum number of subtree values to return.
    ///
    /// # Notes
    ///
    /// While Zebra is doing its initial subtree index rebuild, subtrees will become available
    /// starting at the chain tip. This RPC will return an empty list if the `start_index` subtree
    /// exists, but has not been rebuilt yet. This matches `zcashd`'s behaviour when subtrees aren't
    /// available yet. (But `zcashd` does its rebuild before syncing any blocks.)
    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtrees, Self::Error>;

    /// Returns the raw transaction data, as a [`GetRawTransaction`] JSON string or structure.
    ///
    /// zcashd reference: [`getrawtransaction`](https://zcash.github.io/rpc/getrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required, example="mytxid") The transaction ID of the transaction to be returned.
    /// - `verbose`: (number, optional, default=0, example=1) If 0, return a string of hex-encoded data, otherwise return a JSON object.
    ///
    /// # Notes
    ///
    /// We don't currently support the `blockhash` parameter since lightwalletd does not
    /// use it.
    ///
    /// In verbose mode, we only expose the `hex` and `height` fields since
    /// lightwalletd uses only those:
    /// <https://github.com/zcash/lightwalletd/blob/631bb16404e3d8b045e74a7c5489db626790b2f6/common/common.go#L119>
    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error>;

    /// Returns the transaction ids made by the provided transparent addresses.
    ///
    /// zcashd reference: [`getaddresstxids`](https://zcash.github.io/rpc/getaddresstxids.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"], \"start\": 1000, \"end\": 2000}) A struct with the following named fields:
    ///     - `addresses`: (json array of string, required) The addresses to get transactions from.
    ///     - `start`: (numeric, required) The lower height to start looking for transactions (inclusive).
    ///     - `end`: (numeric, required) The top height to stop looking for transactions (inclusive).
    ///
    /// # Notes
    ///
    /// Only the multi-argument format is used by lightwalletd and this is what we currently support:
    /// <https://github.com/zcash/lightwalletd/blob/631bb16404e3d8b045e74a7c5489db626790b2f6/common/common.go#L97-L102>
    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error>;

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// zcashd reference: [`getaddressutxos`](https://zcash.github.io/rpc/getaddressutxos.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `addresses`: (array, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"]}) The addresses to get outputs from.
    ///
    /// # Notes
    ///
    /// lightwalletd always uses the multi-address request, without chaininfo:
    /// <https://github.com/zcash/lightwalletd/blob/master/frontend/service.go#L402>
    async fn z_get_address_utxos(
        &self,
        address_strings: AddressStrings,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error>;

    /// Helper function to get the chain height
    async fn chain_height(&self) -> Result<Height, Self::Error>;

    /// Helper function, to get the list of taddresses that have sends or reciepts
    /// within a given block range
    async fn get_taddress_txids_helper(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<Vec<String>, Self::Error> {
        let chain_height = self.chain_height().await?;
        let (start, end) = match request.range {
            Some(range) => match (range.start, range.end) {
                (Some(start), Some(end)) => {
                    let start = match u32::try_from(start.height) {
                        Ok(height) => height.min(chain_height.0),
                        Err(_) => {
                            return Err(Self::Error::from(tonic::Status::invalid_argument(
                                "Error: Start height out of range. Failed to convert to u32.",
                            )))
                        }
                    };
                    let end = match u32::try_from(end.height) {
                        Ok(height) => height.min(chain_height.0),
                        Err(_) => {
                            return Err(Self::Error::from(tonic::Status::invalid_argument(
                                "Error: End height out of range. Failed to convert to u32.",
                            )))
                        }
                    };
                    if start > end {
                        (end, start)
                    } else {
                        (start, end)
                    }
                }
                _ => {
                    return Err(Self::Error::from(tonic::Status::invalid_argument(
                        "Error: Incomplete block range given.",
                    )))
                }
            },
            None => {
                return Err(Self::Error::from(tonic::Status::invalid_argument(
                    "Error: No block range given.",
                )))
            }
        };
        self.get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
            vec![request.address],
            start,
            end,
        ))
        .await
    }
}

/// LightWallet RPC method signatures.
///
/// Doc comments taken from Zaino-Proto for consistency.
#[async_trait]
pub trait LightWalletIndexer: Send + Sync + Clone + ZcashIndexer + 'static {
    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error>;

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error>;

    /// Same as GetBlock except actions contain only nullifiers
    async fn get_block_nullifiers(&self, request: BlockId) -> Result<CompactBlock, Self::Error>;

    /// Return a list of consecutive compact blocks
    async fn get_block_range(&self, request: BlockRange)
        -> Result<CompactBlockStream, Self::Error>;

    /// Same as GetBlockRange except actions contain only nullifiers
    async fn get_block_range_nullifiers(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error>;

    /// Return the requested full (not compact) transaction (as from zcashd)
    async fn get_transaction(&self, request: TxFilter) -> Result<RawTransaction, Self::Error>;

    /// Submit the given transaction to the Zcash network
    async fn send_transaction(&self, request: RawTransaction) -> Result<SendResponse, Self::Error>;

    /// Return the txids corresponding to the given t-address within the given block range
    async fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error>;

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(&self, request: AddressList) -> Result<Balance, Self::Error>;

    /// Returns the total balance for a list of taddrs
    ///
    /// TODO: Update input type.
    async fn get_taddress_balance_stream(
        &self,
        request: AddressStream,
    ) -> Result<Balance, Self::Error>;

    /// Return the compact transactions currently in the mempool; the results
    /// can be a few seconds out of date. If the Exclude list is empty, return
    /// all transactions; otherwise return all *except* those in the Exclude list
    /// (if any); this allows the client to avoid receiving transactions that it
    /// already has (from an earlier call to this rpc). The transaction IDs in the
    /// Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    async fn get_mempool_tx(
        &self,
        request: Exclude,
    ) -> Result<CompactTransactionStream, Self::Error>;

    /// Return a stream of current Mempool transactions. This will keep the output stream open while
    /// there are mempool transactions. It will close the returned stream when a new block is mined.
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error>;

    /// GetTreeState returns the note commitment tree state corresponding to the given block.
    /// See section 3.7 of the Zcash protocol specification. It returns several other useful
    /// values also (even though they can be obtained using GetBlock).
    /// The block can be specified by either height or hash.
    async fn get_tree_state(&self, request: BlockId) -> Result<TreeState, Self::Error>;

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error>;

    /// Helper function to get timeout and channel size from config
    fn timeout_channel_size(&self) -> (u32, u32);

    /// Returns a stream of information about roots of subtrees of the Sapling and Orchard
    /// note commitment trees.
    async fn get_subtree_roots(
        &self,
        request: GetSubtreeRootsArg,
    ) -> Result<SubtreeRootReplyStream, <Self as ZcashIndexer>::Error> {
        let pool = match ShieldedProtocol::try_from(request.shielded_protocol) {
            Ok(protocol) => protocol.as_str_name(),
            Err(_) => {
                return Err(<Self as ZcashIndexer>::Error::from(
                    tonic::Status::invalid_argument("Error: Invalid shielded protocol value."),
                ))
            }
        };
        let start_index = match u16::try_from(request.start_index) {
            Ok(value) => value,
            Err(_) => {
                return Err(<Self as ZcashIndexer>::Error::from(
                    tonic::Status::invalid_argument("Error: start_index value exceeds u16 range."),
                ))
            }
        };
        let limit = if request.max_entries == 0 {
            None
        } else {
            match u16::try_from(request.max_entries) {
                Ok(value) => Some(value),
                Err(_) => {
                    return Err(<Self as ZcashIndexer>::Error::from(
                        tonic::Status::invalid_argument(
                            "Error: max_entries value exceeds u16 range.",
                        ),
                    ))
                }
            }
        };
        let service_clone = self.clone();
        let subtrees = service_clone
            .z_get_subtrees_by_index(
                pool.to_string(),
                NoteCommitmentSubtreeIndex(start_index),
                limit.map(NoteCommitmentSubtreeIndex),
            )
            .await?;
        let (service_timeout, service_channel_size) = self.timeout_channel_size();
        let (channel_tx, channel_rx) = mpsc::channel(service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                std::time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for subtree in subtrees.subtrees {
                        match service_clone
                            .z_get_block(subtree.end_height.0.to_string(), Some(1))
                            .await
                        {
                            Ok(GetBlock::Object { hash, height, .. }) => {
                                let checked_height = match height {
                                    Some(h) => h.0 as u64,
                                    None => {
                                        match channel_tx
                                            .send(Err(tonic::Status::unknown(
                                                "Error: No block height returned by node.",
                                            )))
                                            .await
                                        {
                                            Ok(_) => break,
                                            Err(e) => {
                                                warn!(
                                                    "GetSubtreeRoots channel closed unexpectedly: {}",
                                                    e
                                                );
                                                break;
                                            }
                                        }
                                    }
                                };
                                let checked_root_hash = match hex::decode(&subtree.root) {
                                    Ok(hash) => hash,
                                    Err(e) => {
                                        match channel_tx
                                            .send(Err(tonic::Status::unknown(format!(
                                                "Error: Failed to hex decode root hash: {}.",
                                                e
                                            ))))
                                            .await
                                        {
                                            Ok(_) => break,
                                            Err(e) => {
                                                warn!(
                                                    "GetSubtreeRoots channel closed unexpectedly: {}",
                                                    e
                                                );
                                                break;
                                            }
                                        }
                                    }
                                };
                                if channel_tx
                                    .send(Ok(SubtreeRoot {
                                        root_hash: checked_root_hash,
                                        completing_block_hash: hash
                                            .0
                                            .bytes_in_display_order()
                                            .to_vec(),
                                        completing_block_height: checked_height,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(GetBlock::Raw(_)) => {
                                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                if channel_tx
                                .send(Err(tonic::Status::unknown(
                                    "Error: Received raw block type, this should not be possible.",
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            }
                            Err(e) => {
                                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                if channel_tx
                                    .send(Err(tonic::Status::unknown(format!(
                                        "Error: Could not fetch block at height [{}] from node: {}",
                                        subtree.end_height.0, e
                                    ))))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                },
            )
            .await;
            match timeout {
                Ok(_) => {}
                Err(_) => {
                    channel_tx
                        .send(Err(tonic::Status::deadline_exceeded(
                            "Error: get_mempool_stream gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(SubtreeRootReplyStream::new(channel_rx))
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error>;

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are returned in a stream.
    async fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error>;

    /// Return information about this lightwalletd instance and the blockchain
    async fn get_lightd_info(&self) -> Result<LightdInfo, Self::Error>;

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production)
    ///
    /// NOTE: Currently unimplemented in Zaino.
    async fn ping(&self, request: Duration) -> Result<PingResponse, Self::Error>;
}
/// Zcash Service functionality.
#[async_trait]
pub trait LightWalletService: Sized + ZcashService<Subscriber: LightWalletIndexer> {}

impl<T> LightWalletService for T where T: ZcashService {}

pub(crate) async fn handle_raw_transaction<Indexer: LightWalletIndexer>(
    chain_height: u64,
    transaction: Result<GetRawTransaction, Indexer::Error>,
    transmitter: mpsc::Sender<Result<RawTransaction, tonic::Status>>,
) -> Result<(), mpsc::error::SendError<Result<RawTransaction, tonic::Status>>> {
    match transaction {
        Ok(GetRawTransaction::Object(transaction_obj)) => {
            let height: u64 = match transaction_obj.height {
                Some(h) => h as u64,
                // Zebra returns None for mempool transactions, convert to `Mempool Height`.
                None => chain_height,
            };
            transmitter
                .send(Ok(RawTransaction {
                    data: transaction_obj.hex.as_ref().to_vec(),
                    height,
                }))
                .await
        }
        Ok(GetRawTransaction::Raw(_)) => {
            transmitter
                .send(Err(tonic::Status::unknown(
                    "Received raw transaction type, this should not be impossible.",
                )))
                .await
        }
        Err(e) => {
            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
            transmitter
                .send(Err(tonic::Status::unknown(e.to_string())))
                .await
        }
    }
}
