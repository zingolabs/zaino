// currently this is a direct copy with no modifications
// from zallet `main` @ 3a36f3d
use async_trait::async_trait;
use jsonrpsee::{
    core::{JsonValue, RpcResult},
    proc_macros::rpc,
};
use serde::Serialize;
use tokio::sync::RwLock;
use zaino_state::FetchServiceSubscriber;

use crate::components::{
    chain_view::ChainView,
    database::{Database, DbHandle},
    keystore::KeyStore,
};

use super::asyncop::{AsyncOperation, ContextInfo, OperationId};

mod get_address_for_account;
mod get_new_account;
mod get_notes_count;
mod get_operation;
mod get_wallet_info;
mod help;
mod list_accounts;
mod list_addresses;
mod list_operation_ids;
mod list_unified_receivers;
mod list_unspent;
mod lock_wallet;
mod openrpc;
mod recover_accounts;
mod stop;
mod unlock_wallet;
mod view_transaction;
mod z_get_total_balance;
mod z_send_many;

#[rpc(server)]
pub(crate) trait Rpc {
    /// List all commands, or get help for a specified command.
    ///
    /// # Arguments
    /// - `command` (string, optional) The command to get help on.
    #[method(name = "help")]
    fn help(&self, command: Option<&str>) -> help::Response;

    /// Returns an OpenRPC schema as a description of this service.
    #[method(name = "rpc.discover")]
    fn openrpc(&self) -> openrpc::Response;

    /// Returns the list of operation ids currently known to the wallet.
    ///
    /// # Arguments
    /// - `status` (string, optional) Filter result by the operation's state e.g. "success".
    #[method(name = "z_listoperationids")]
    async fn list_operation_ids(&self, status: Option<&str>) -> list_operation_ids::Response;

    /// Get operation status and any associated result or error data.
    ///
    /// The operation will remain in memory.
    ///
    /// - If the operation has failed, it will include an error object.
    /// - If the operation has succeeded, it will include the result value.
    /// - If the operation was cancelled, there will be no error object or result value.
    ///
    /// # Arguments
    /// - `operationid` (array, optional) A list of operation ids we are interested in.
    ///   If not provided, examine all operations known to the node.
    #[method(name = "z_getoperationstatus")]
    async fn get_operation_status(&self, operationid: Vec<OperationId>) -> get_operation::Response;

    /// Retrieve the result and status of an operation which has finished, and then remove
    /// the operation from memory.
    ///
    /// - If the operation has failed, it will include an error object.
    /// - If the operation has succeeded, it will include the result value.
    /// - If the operation was cancelled, there will be no error object or result value.
    ///
    /// # Arguments
    /// - `operationid` (array, optional) A list of operation ids we are interested in.
    ///   If not provided, retrieve all finished operations known to the node.
    #[method(name = "z_getoperationresult")]
    async fn get_operation_result(&self, operationid: Vec<OperationId>) -> get_operation::Response;

    /// Returns wallet state information.
    #[method(name = "getwalletinfo")]
    async fn get_wallet_info(&self) -> get_wallet_info::Response;

    /// Stores the wallet decryption key in memory for `timeout` seconds.
    ///
    /// If the wallet is locked, this API must be invoked prior to performing operations
    /// that require the availability of private keys, such as sending funds.
    ///
    /// Issuing the `walletpassphrase` command while the wallet is already unlocked will
    /// set a new unlock time that overrides the old one.
    #[method(name = "walletpassphrase")]
    async fn unlock_wallet(
        &self,
        passphrase: age::secrecy::SecretString,
        timeout: u64,
    ) -> unlock_wallet::Response;

    /// Removes the wallet encryption key from memory, locking the wallet.
    ///
    /// After calling this method, you will need to call `walletpassphrase` again before
    /// being able to call any methods which require the wallet to be unlocked.
    #[method(name = "walletlock")]
    async fn lock_wallet(&self) -> lock_wallet::Response;

    #[method(name = "z_listaccounts")]
    async fn list_accounts(&self) -> list_accounts::Response;

    /// Prepares and returns a new account.
    ///
    /// If the wallet contains more than one UA-compatible HD seed phrase, the `seedfp`
    /// argument must be provided. Available seed fingerprints can be found in the output
    /// of the `listaddresses` RPC method.
    ///
    /// Within a UA-compatible HD seed phrase, accounts are numbered starting from zero;
    /// this RPC method selects the next available sequential account number.
    ///
    /// Each new account is a separate group of funds within the wallet, and adds an
    /// additional performance cost to wallet scanning.
    ///
    /// Use the `z_getaddressforaccount` RPC method to obtain addresses for an account.
    #[method(name = "z_getnewaccount")]
    async fn get_new_account(
        &self,
        account_name: &str,
        seedfp: Option<&str>,
    ) -> get_new_account::Response;

    /// Tells the wallet to track specific accounts.
    ///
    /// Returns the UUIDs within this Zallet instance of the newly-tracked accounts.
    /// Accounts that are already tracked by the wallet are ignored.
    ///
    /// After calling this method, a subsequent call to `z_getnewaccount` will add the
    /// first account with index greater than all indices provided here for the
    /// corresponding `seedfp` (as well as any already tracked by the wallet).
    ///
    /// Each tracked account is a separate group of funds within the wallet, and adds an
    /// additional performance cost to wallet scanning.
    ///
    /// Use the `z_getaddressforaccount` RPC method to obtain addresses for an account.
    ///
    /// # Arguments
    ///
    /// - `accounts` (array, required) An array of JSON objects representing the accounts
    ///   to recover, with the following fields:
    ///   - `name` (string, required)
    ///   - `seedfp` (string, required) The seed fingerprint for the mnemonic phrase from
    ///     which the account is derived. Available seed fingerprints can be found in the
    ///     output of the `listaddresses` RPC method.
    ///   - `zip32_account_index` (numeric, required)
    ///   - `birthday_height` (numeric, required)
    #[method(name = "z_recoveraccounts")]
    async fn recover_accounts(
        &self,
        accounts: Vec<recover_accounts::AccountParameter<'_>>,
    ) -> recover_accounts::Response;

    /// For the given account, derives a Unified Address in accordance with the remaining
    /// arguments:
    ///
    /// - If no list of receiver types is given (or the empty list `[]`), the best and
    ///   second-best shielded receiver types, along with the "p2pkh" (i.e. transparent)
    ///   receiver type, will be used.
    /// - If no diversifier index is given, then:
    ///   - If a transparent receiver would be included (either because no list of
    ///     receiver types is given, or the provided list includes "p2pkh"), the next
    ///     unused index (that is valid for the list of receiver types) will be selected.
    ///   - If only shielded receivers would be included (because a list of receiver types
    ///     is given that does not include "p2pkh"), a time-based index will be selected.
    ///
    /// The account parameter must be a UUID or account number that was previously
    /// generated by a call to the `z_getnewaccount` RPC method. The legacy account number
    /// is only supported for wallets containing a single seed phrase.
    ///
    /// Once a Unified Address has been derived at a specific diversifier index,
    /// re-deriving it (via a subsequent call to `z_getaddressforaccount` with the same
    /// account and index) will produce the same address with the same list of receiver
    /// types. An error will be returned if a different list of receiver types is
    /// requested, including when the empty list `[]` is provided (if the default receiver
    /// types don't match).
    #[method(name = "z_getaddressforaccount")]
    async fn get_address_for_account(
        &self,
        account: JsonValue,
        receiver_types: Option<Vec<String>>,
        diversifier_index: Option<u128>,
    ) -> get_address_for_account::Response;

    /// Lists the addresses managed by this wallet by source.
    ///
    /// Sources include:
    /// - Addresses generated from randomness by a legacy `zcashd` wallet.
    /// - Sapling addresses generated from the legacy `zcashd` HD seed.
    /// - Imported watchonly transparent addresses.
    /// - Shielded addresses tracked using imported viewing keys.
    /// - Addresses derived from mnemonic seed phrases.
    ///
    /// In the case that a source does not have addresses for a value pool, the key
    /// associated with that pool will be absent.
    ///
    /// REMINDER: It is recommended that you back up your wallet files regularly. If you
    /// have not imported externally-produced keys, it only necessary to have backed up
    /// the wallet's key storage file.
    #[method(name = "listaddresses")]
    async fn list_addresses(&self) -> list_addresses::Response;

    /// Returns the total value of funds stored in the node's wallet.
    ///
    /// TODO: Currently watchonly addresses cannot be omitted; `includeWatchonly` must be
    /// set to true.
    ///
    /// # Arguments
    ///
    /// - `minconf` (numeric, optional, default=1) Only include private and transparent
    ///   transactions confirmed at least this many times.
    /// - `includeWatchonly` (bool, optional, default=false) Also include balance in
    ///   watchonly addresses (see 'importaddress' and 'z_importviewingkey').
    #[method(name = "z_gettotalbalance")]
    async fn z_get_total_balance(
        &self,
        minconf: Option<u32>,
        #[argument(rename = "includeWatchonly")] include_watch_only: Option<bool>,
    ) -> z_get_total_balance::Response;

    #[method(name = "z_listunifiedreceivers")]
    fn list_unified_receivers(&self, unified_address: &str) -> list_unified_receivers::Response;

    /// Returns detailed shielded information about in-wallet transaction `txid`.
    #[method(name = "z_viewtransaction")]
    async fn view_transaction(&self, txid: &str) -> view_transaction::Response;

    /// Returns an array of unspent shielded notes with between minconf and maxconf
    /// (inclusive) confirmations.
    ///
    /// Results may be optionally filtered to only include notes sent to specified
    /// addresses. When `minconf` is 0, unspent notes with zero confirmations are
    /// returned, even though they are not immediately spendable.
    ///
    /// # Arguments
    /// - `minconf` (default = 1)
    #[method(name = "z_listunspent")]
    async fn list_unspent(&self) -> list_unspent::Response;

    #[method(name = "z_getnotescount")]
    async fn get_notes_count(
        &self,
        minconf: Option<u32>,
        as_of_height: Option<i32>,
    ) -> get_notes_count::Response;

    /// Send a transaction with multiple recipients.
    ///
    /// This is an async operation; it returns an operation ID string that you can pass to
    /// `z_getoperationstatus` or `z_getoperationresult`.
    ///
    /// Amounts are decimal numbers with at most 8 digits of precision.
    ///
    /// Change generated from one or more transparent addresses flows to a new transparent
    /// address, while change generated from a legacy Sapling address returns to itself.
    /// TODO: https://github.com/zcash/wallet/issues/138
    ///
    /// When sending from a unified address, change is returned to the internal-only
    /// address for the associated unified account.
    ///
    /// When spending coinbase UTXOs, only shielded recipients are permitted and change is
    /// not allowed; the entire value of the coinbase UTXO(s) must be consumed.
    /// TODO: https://github.com/zcash/wallet/issues/137
    ///
    /// # Arguments
    ///
    /// - `fromaddress` (string, required) The transparent or shielded address to send the
    ///   funds from. The following special strings are also accepted:
    ///   - `"ANY_TADDR"`: Select non-coinbase UTXOs from any transparent addresses
    ///     belonging to the wallet. Use `z_shieldcoinbase` to shield coinbase UTXOs from
    ///     multiple transparent addresses.
    ///   If a unified address is provided for this argument, the TXOs to be spent will be
    ///   selected from those associated with the account corresponding to that unified
    ///   address, from value pools corresponding to the receivers included in the UA.
    /// - `amounts` (array, required) An array of JSON objects representing the amounts to
    ///   send, with the following fields:
    ///   - `address` (string, required) A taddr, zaddr, or Unified Address.
    ///   - `amount` (numeric, required) The numeric amount in ZEC.
    ///   - `memo` (string, optional) If the address is a zaddr, raw data represented in
    ///     hexadecimal string format. If the output is being sent to a transparent
    ///     address, it’s an error to include this field.
    /// - `minconf` (numeric, optional) Only use funds confirmed at least this many times.
    /// - `fee` (numeric, optional) If set, it must be null. Zallet always uses a fee
    ///   calculated according to ZIP 317.
    /// - `privacyPolicy` (string, optional, default=`"FullPrivacy"`) Policy for what
    ///   information leakage is acceptable. One of the following strings:
    ///   - `"FullPrivacy"`: Only allow fully-shielded transactions (involving a single
    ///     shielded value pool).
    ///   - `"AllowRevealedAmounts"`: Allow funds to cross between shielded value pools,
    ///     revealing the amount that crosses pools.
    ///   - `"AllowRevealedRecipients"`: Allow transparent recipients. This also implies
    ///     revealing information described under `"AllowRevealedAmounts"`.
    ///   - `"AllowRevealedSenders"`: Allow transparent funds to be spent, revealing the
    ///     sending addresses and amounts. This implies revealing information described
    ///     under `"AllowRevealedAmounts"`.
    ///   - `"AllowFullyTransparent"`: Allow transaction to both spend transparent funds
    ///     and have transparent recipients. This implies revealing information described
    ///     under `"AllowRevealedSenders"` and `"AllowRevealedRecipients"`.
    ///   - `"AllowLinkingAccountAddresses"`: Allow selecting transparent coins from the
    ///     full account, rather than just the funds sent to the transparent receiver in
    ///     the provided Unified Address. This implies revealing information described
    ///     under `"AllowRevealedSenders"`.
    ///   - `"NoPrivacy"`: Allow the transaction to reveal any information necessary to
    ///     create it. This implies revealing information described under
    ///     `"AllowFullyTransparent"` and `"AllowLinkingAccountAddresses"`.
    #[method(name = "z_sendmany")]
    async fn z_send_many(
        &self,
        fromaddress: String,
        amounts: Vec<z_send_many::AmountParameter>,
        minconf: Option<u32>,
        fee: Option<JsonValue>,
        #[argument(rename = "privacyPolicy")] privacy_policy: Option<String>,
    ) -> z_send_many::Response;

    /// Stop the running zallet process.
    ///
    /// # Notes
    ///
    /// - Works for non windows targets only.
    /// - Works only if the network of the running zallet process is `Regtest`.
    #[method(name = "stop")]
    async fn stop(&self) -> stop::Response;
}

pub(crate) struct RpcImpl {
    wallet: Database,
    keystore: KeyStore,
    chain_view: ChainView,
    async_ops: RwLock<Vec<AsyncOperation>>,
}

impl RpcImpl {
    /// Creates a new instance of the RPC handler.
    pub(crate) fn new(wallet: Database, keystore: KeyStore, chain_view: ChainView) -> Self {
        Self {
            wallet,
            keystore,
            chain_view,
            async_ops: RwLock::new(Vec::new()),
        }
    }

    async fn wallet(&self) -> RpcResult<DbHandle> {
        self.wallet
            .handle()
            .await
            .map_err(|_| jsonrpsee::types::ErrorCode::InternalError.into())
    }

    async fn chain(&self) -> RpcResult<FetchServiceSubscriber> {
        self.chain_view
            .subscribe()
            .await
            .map(|s| s.inner())
            .map_err(|_| jsonrpsee::types::ErrorCode::InternalError.into())
    }

    async fn start_async<F, T>(&self, (context, f): (Option<ContextInfo>, F)) -> OperationId
    where
        F: Future<Output = RpcResult<T>> + Send + 'static,
        T: Serialize + Send + 'static,
    {
        let mut async_ops = self.async_ops.write().await;
        let op = AsyncOperation::new(context, f).await;
        let op_id = op.operation_id().clone();
        async_ops.push(op);
        op_id
    }
}

#[async_trait]
impl RpcServer for RpcImpl {
    fn help(&self, command: Option<&str>) -> help::Response {
        help::call(command)
    }

    fn openrpc(&self) -> openrpc::Response {
        openrpc::call()
    }

    async fn list_operation_ids(&self, status: Option<&str>) -> list_operation_ids::Response {
        list_operation_ids::call(&self.async_ops.read().await, status).await
    }

    async fn get_operation_status(&self, operationid: Vec<OperationId>) -> get_operation::Response {
        get_operation::status(&self.async_ops.read().await, operationid).await
    }

    async fn get_operation_result(&self, operationid: Vec<OperationId>) -> get_operation::Response {
        get_operation::result(self.async_ops.write().await.as_mut(), operationid).await
    }

    async fn get_wallet_info(&self) -> get_wallet_info::Response {
        get_wallet_info::call(&self.keystore).await
    }

    async fn unlock_wallet(
        &self,
        passphrase: age::secrecy::SecretString,
        timeout: u64,
    ) -> unlock_wallet::Response {
        unlock_wallet::call(&self.keystore, passphrase, timeout).await
    }

    async fn lock_wallet(&self) -> lock_wallet::Response {
        lock_wallet::call(&self.keystore).await
    }

    async fn list_accounts(&self) -> list_accounts::Response {
        list_accounts::call(self.wallet().await?.as_ref())
    }

    async fn get_new_account(
        &self,
        account_name: &str,
        seedfp: Option<&str>,
    ) -> get_new_account::Response {
        get_new_account::call(
            self.wallet().await?.as_mut(),
            &self.keystore,
            self.chain().await?,
            account_name,
            seedfp,
        )
        .await
    }

    async fn recover_accounts(
        &self,
        accounts: Vec<recover_accounts::AccountParameter<'_>>,
    ) -> recover_accounts::Response {
        recover_accounts::call(
            self.wallet().await?.as_mut(),
            &self.keystore,
            self.chain().await?,
            accounts,
        )
        .await
    }

    async fn get_address_for_account(
        &self,
        account: JsonValue,
        receiver_types: Option<Vec<String>>,
        diversifier_index: Option<u128>,
    ) -> get_address_for_account::Response {
        get_address_for_account::call(
            self.wallet().await?.as_mut(),
            account,
            receiver_types,
            diversifier_index,
        )
    }

    async fn list_addresses(&self) -> list_addresses::Response {
        list_addresses::call(self.wallet().await?.as_ref())
    }

    async fn z_get_total_balance(
        &self,
        minconf: Option<u32>,
        include_watch_only: Option<bool>,
    ) -> z_get_total_balance::Response {
        z_get_total_balance::call(self.wallet().await?.as_ref(), minconf, include_watch_only)
    }

    fn list_unified_receivers(&self, unified_address: &str) -> list_unified_receivers::Response {
        list_unified_receivers::call(unified_address)
    }

    async fn view_transaction(&self, txid: &str) -> view_transaction::Response {
        view_transaction::call(self.wallet().await?.as_ref(), txid)
    }

    async fn list_unspent(&self) -> list_unspent::Response {
        list_unspent::call(self.wallet().await?.as_ref())
    }

    async fn get_notes_count(
        &self,
        minconf: Option<u32>,
        as_of_height: Option<i32>,
    ) -> get_notes_count::Response {
        get_notes_count::call(self.wallet().await?.as_ref(), minconf, as_of_height)
    }

    async fn z_send_many(
        &self,
        fromaddress: String,
        amounts: Vec<z_send_many::AmountParameter>,
        minconf: Option<u32>,
        fee: Option<JsonValue>,
        privacy_policy: Option<String>,
    ) -> z_send_many::Response {
        Ok(self
            .start_async(
                z_send_many::call(
                    self.wallet().await?,
                    self.keystore.clone(),
                    self.chain().await?,
                    fromaddress,
                    amounts,
                    minconf,
                    fee,
                    privacy_policy,
                )
                .await?,
            )
            .await)
    }

    async fn stop(&self) -> stop::Response {
        stop::call(self.wallet().await?)
    }
}
