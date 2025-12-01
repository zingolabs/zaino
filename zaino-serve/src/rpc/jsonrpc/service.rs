//! Zcash RPC implementations.

use zaino_fetch::jsonrpsee::response::block_deltas::BlockDeltas;
use zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader;
use zaino_fetch::jsonrpsee::response::block_subsidy::GetBlockSubsidy;
use zaino_fetch::jsonrpsee::response::mining_info::GetMiningInfoWire;
use zaino_fetch::jsonrpsee::response::peer_info::GetPeerInfo;
use zaino_fetch::jsonrpsee::response::{GetMempoolInfoResponse, GetNetworkSolPsResponse};
use zaino_state::{LightWalletIndexer, ZcashIndexer};

use zebra_chain::{block::Height, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::client::{
    GetBlockchainInfoResponse, GetSubtreesByIndexResponse, GetTreestateResponse,
    ValidateAddressResponse,
};
use zebra_rpc::methods::{
    AddressBalance, AddressStrings, GetAddressTxIdsRequest, GetAddressUtxos, GetBlock,
    GetBlockHash, GetInfo, GetRawTransaction, SentTransactionHash,
};

use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{proc_macros::rpc, types::ErrorCode};

use crate::rpc::JsonRpcClient;

/// Zcash RPC method signatures.
///
/// Doc comments taken from Zebra for consistency.
#[rpc(server)]
pub trait ZcashIndexerRpc {
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
    #[method(name = "getinfo")]
    async fn get_info(&self) -> Result<GetInfo, ErrorObjectOwned>;

    /// Returns blockchain state information, as a [`GetBlockchainInfoResponse`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetBlockchainInfoResponse`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L72-L89)
    #[method(name = "getblockchaininfo")]
    async fn get_blockchain_info(&self) -> Result<GetBlockchainInfoResponse, ErrorObjectOwned>;

    /// Returns details on the active state of the TX memory pool.
    ///
    /// online zcash rpc reference: [`getmempoolinfo`](https://zcash.github.io/rpc/getmempoolinfo.html)
    /// method: post
    /// tags: mempool
    ///
    /// Canonical source code implementation: [`getmempoolinfo`](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/blockchain.cpp#L1555)
    #[method(name = "getmempoolinfo")]
    async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, ErrorObjectOwned>;

    /// Returns a json object containing mining-related information.
    ///
    /// `zcashd` reference (may be outdated): [`getmininginfo`](https://zcash.github.io/rpc/getmininginfo.html)
    #[method(name = "getmininginfo")]
    async fn get_mining_info(&self) -> Result<GetMiningInfoWire, ErrorObjectOwned>;

    /// Returns the hash of the best block (tip) of the longest chain.
    /// zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// The zcashd doc reference above says there are no parameters and the result is a "hex" (string) of the block hash hex encoded.
    /// The Zcash source code is considered canonical:
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339)returning a `std::string`
    #[method(name = "getbestblockhash")]
    async fn get_best_blockhash(&self) -> Result<GetBlockHash, ErrorObjectOwned>;

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    #[method(name = "getdifficulty")]
    async fn get_difficulty(&self) -> Result<f64, ErrorObjectOwned>;

    /// Returns information about the given block and its transactions.
    ///
    /// zcashd reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    /// method: post
    /// tags: blockchain
    #[method(name = "getblockdeltas")]
    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, ErrorObjectOwned>;

    /// Returns data about each connected network node as a json array of objects.
    ///
    /// zcashd reference: [`getpeerinfo`](https://zcash.github.io/rpc/getpeerinfo.html)
    /// tags: network
    ///
    /// Current `zebrad` does not include the same fields as `zcashd`.
    #[method(name = "getpeerinfo")]
    async fn get_peer_info(&self) -> Result<GetPeerInfo, ErrorObjectOwned>;

    /// Returns block subsidy reward, taking into account the mining slow start and the founders reward, of block at index provided.
    ///
    /// zcashd reference: [`getblocksubsidy`](https://zcash.github.io/rpc/getblocksubsidy.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `height`: (number, optional) The block height. If not provided, defaults to the current height of the chain.
    #[method(name = "getblocksubsidy")]
    async fn get_block_subsidy(&self, height: u32) -> Result<GetBlockSubsidy, ErrorObjectOwned>;

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    #[method(name = "getblockcount")]
    async fn get_block_count(&self) -> Result<Height, ErrorObjectOwned>;

    /// Return information about the given Zcash address.
    ///
    /// # Parameters
    /// - `address`: (string, required, example="tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb") The Zcash transparent address to validate.
    ///
    /// zcashd reference: [`validateaddress`](https://zcash.github.io/rpc/validateaddress.html)
    /// method: post
    /// tags: blockchain
    #[method(name = "validateaddress")]
    async fn validate_address(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, ErrorObjectOwned>;

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
    #[method(name = "getaddressbalance")]
    async fn z_get_address_balance(
        &self,
        address_strings: AddressStrings,
    ) -> Result<AddressBalance, ErrorObjectOwned>;

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
    #[method(name = "sendrawtransaction")]
    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, ErrorObjectOwned>;

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
    #[method(name = "getblock")]
    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, ErrorObjectOwned>;

    /// If verbose is false, returns a string that is serialized, hex-encoded data for blockheader `hash`.
    /// If verbose is true, returns an Object with information about blockheader `hash`.
    ///
    /// # Parameters
    ///
    /// - hash: (string, required) The block hash
    /// - verbose: (boolean, optional, default=true) true for a json object, false for the hex encoded data
    ///
    /// zcashd reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    /// zcashd implementation [here](https://github.com/zcash/zcash/blob/16ac743764a513e41dafb2cd79c2417c5bb41e81/src/rpc/blockchain.cpp#L668)
    ///
    /// method: post
    /// tags: blockchain
    #[method(name = "getblockheader")]
    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> Result<GetBlockHeader, ErrorObjectOwned>;

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    #[method(name = "getrawmempool")]
    async fn get_raw_mempool(&self) -> Result<Vec<String>, ErrorObjectOwned>;

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
    #[method(name = "z_gettreestate")]
    async fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, ErrorObjectOwned>;

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
    #[method(name = "z_getsubtreesbyindex")]
    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtreesByIndexResponse, ErrorObjectOwned>;

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
    #[method(name = "getrawtransaction")]
    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, ErrorObjectOwned>;

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
    #[method(name = "getaddresstxids")]
    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, ErrorObjectOwned>;

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
    #[method(name = "getaddressutxos")]
    async fn z_get_address_utxos(
        &self,
        address_strings: AddressStrings,
    ) -> Result<Vec<GetAddressUtxos>, ErrorObjectOwned>;

    /// Returns the estimated network solutions per second based on the last n blocks.
    ///
    /// zcashd reference: [`getnetworksolps`](https://zcash.github.io/rpc/getnetworksolps.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `blocks`: (number, optional, default=120) Number of blocks, or -1 for blocks over difficulty averaging window.
    /// - `height`: (number, optional, default=-1) To estimate network speed at the time of a specific block height.
    #[method(name = "getnetworksolps")]
    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<GetNetworkSolPsResponse, ErrorObjectOwned>;
}
/// Uses ErrorCode::InvalidParams as this is converted to zcash legacy "minsc" ErrorCode in RPC middleware.
#[jsonrpsee::core::async_trait]
impl<Indexer: ZcashIndexer + LightWalletIndexer> ZcashIndexerRpcServer for JsonRpcClient<Indexer> {
    async fn get_info(&self) -> Result<GetInfo, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_info()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_mining_info(&self) -> Result<GetMiningInfoWire, ErrorObjectOwned> {
        Ok(self
            .service_subscriber
            .inner_ref()
            .get_mining_info()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })?)
    }

    async fn get_best_blockhash(&self) -> Result<GetBlockHash, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_best_blockhash()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_blockchain_info(&self) -> Result<GetBlockchainInfoResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_blockchain_info()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_mempool_info()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_difficulty(&self) -> Result<f64, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_difficulty()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_block_deltas(hash)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_peer_info(&self) -> Result<GetPeerInfo, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_peer_info()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<GetBlockSubsidy, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_block_subsidy(height)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_block_count(&self) -> Result<Height, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_block_count()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn validate_address(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .validate_address(address)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn z_get_address_balance(
        &self,
        address_strings: AddressStrings,
    ) -> Result<AddressBalance, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .z_get_address_balance(address_strings)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .send_raw_transaction(raw_transaction_hex)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .z_get_block(hash_or_height, verbosity)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> Result<GetBlockHeader, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_block_header(hash, verbose)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_raw_mempool(&self) -> Result<Vec<String>, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_raw_mempool()
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .z_get_treestate(hash_or_height)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtreesByIndexResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .z_get_subtrees_by_index(pool, start_index, limit)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_raw_transaction(txid_hex, verbose)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_address_tx_ids(request)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    async fn z_get_address_utxos(
        &self,
        address_strings: AddressStrings,
    ) -> Result<Vec<GetAddressUtxos>, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .z_get_address_utxos(address_strings)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }

    /// Returns the estimated network solutions per second based on the last n blocks.
    ///
    /// zcashd reference: [`getnetworksolps`](https://zcash.github.io/rpc/getnetworksolps.html)
    /// method: post
    /// tags: blockchain
    ///
    /// This RPC is implemented in the [mining.cpp](https://github.com/zcash/zcash/blob/d00fc6f4365048339c83f463874e4d6c240b63af/src/rpc/mining.cpp#L104)
    /// file of the Zcash repository. The Zebra implementation can be found [here](https://github.com/ZcashFoundation/zebra/blob/19bca3f1159f9cb9344c9944f7e1cb8d6a82a07f/zebra-rpc/src/methods.rs#L2687).
    ///
    /// # Parameters
    ///
    /// - `blocks`: (number, optional, default=120) Number of blocks, or -1 for blocks over difficulty averaging window.
    /// - `height`: (number, optional, default=-1) To estimate network speed at the time of a specific block height.
    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<GetNetworkSolPsResponse, ErrorObjectOwned> {
        self.service_subscriber
            .inner_ref()
            .get_network_sol_ps(blocks, height)
            .await
            .map_err(|e| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    "Internal server error",
                    Some(e.to_string()),
                )
            })
    }
}
