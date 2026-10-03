//! Response bodies, in the field names and encodings zcashd uses.
//!
//! Zatoshi quantities render as integers: `balance` is supply-bounded and fits
//! `u64`, while `received` is a lifetime flow total that is not supply-bounded
//! and so renders from `u128`.

use std::collections::BTreeMap;

use serde::Serialize;

/// The `getmempoolinfo` response.
///
/// zebra and zcashd also report `usage` — the mempool's in-memory cost, distinct
/// from the serialized `bytes`. Zaino builds this summary from the per-transaction
/// metadata of `getrawmempool verbose`, which carries each transaction's
/// serialized size but no memory-cost figure, so `usage` is not computable here
/// and is deliberately absent rather than approximated by `bytes` (which would be
/// a different quantity wearing the same name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MempoolInfoResponse {
    /// Number of transactions in the mempool.
    pub size: u64,
    /// Total serialized size of those transactions, in bytes.
    pub bytes: u64,
}

/// One entry of a verbose `getrawmempool` object.
///
/// `fee` renders as zcashd's ZEC float, with the exact zatoshi integer beside it
/// as `feeZat`. `time` is omitted when the source reports no entry time, never
/// sent as null or zero.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MempoolEntryObject {
    /// Serialized byte length of the transaction.
    pub size: u64,
    /// The transaction's fee, as a ZEC float.
    pub fee: f64,
    /// The transaction's fee in zatoshis — the exact integer beside `fee`.
    #[serde(rename = "feeZat")]
    pub fee_zat: u64,
    /// Unix time (seconds) the transaction entered the mempool, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    /// Chain tip height when the transaction entered the mempool.
    pub height: u32,
}

/// The `getrawmempool` response: an array of txid hex (non-verbose) or an object
/// keyed by txid (verbose).
///
/// Two JSON shapes from one method, chosen by the `verbose` parameter. The
/// verbose form is an object, not an array, because the explorer's warmer
/// pattern-matches its entries as key/value pairs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RawMempoolResponse {
    /// Non-verbose: the txids currently in the mempool, as hex.
    Txids(Vec<String>),
    /// Verbose: each txid mapped to its entry detail.
    Verbose(BTreeMap<String, MempoolEntryObject>),
}

/// The `getaddressbalance` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressBalanceResponse {
    /// Total currently held, in zatoshis.
    pub balance: u64,
    /// Lifetime gross receipts, in zatoshis.
    pub received: u128,
}

/// One entry of the `getaddressdeltas` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltaEntry {
    /// Signed change in zatoshis — negative for a spend.
    pub satoshis: i64,
    /// The transaction that caused the delta, as hex.
    pub txid: String,
    /// Input or output index within the transaction.
    pub index: u32,
    /// Position of the transaction within its block, when the source knows it.
    #[serde(rename = "blockindex", skip_serializing_if = "Option::is_none")]
    pub block_index: Option<u32>,
    /// Block height of the delta.
    pub height: u32,
    /// The transparent address affected.
    pub address: String,
}

/// The range a `chainInfo` request echoes alongside its deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeltaRange {
    /// First height included.
    pub start: u32,
    /// Last height included.
    pub end: u32,
}

/// The `getaddressdeltas` response. `range` appears only when the request asked
/// for `chainInfo` and a query actually ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltasResponse {
    /// The deltas, in `(height, blockindex, index)` order.
    pub deltas: Vec<AddressDeltaEntry>,
    /// The queried range, when `chainInfo` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<DeltaRange>,
}

/// One entry of the `getaddressutxos` list, in zcashd's insight-explorer shape.
///
/// `txid` is display order; `script` is the locking script as hex; `satoshis`
/// is the exact integer value. zcashd's key is `outputIndex` (camel-cased),
/// distinct from the `vout`/`index` of other methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressUtxoEntry {
    /// The transparent address holding the output.
    pub address: String,
    /// The transaction containing the output, as hex (display order).
    pub txid: String,
    /// The output's index within that transaction.
    #[serde(rename = "outputIndex")]
    pub output_index: u32,
    /// The output's locking script, as hex.
    pub script: String,
    /// The output's value, in zatoshis.
    pub satoshis: u64,
    /// Block height at which the output was created.
    pub height: u32,
}

/// The `validateaddress` response. zcashd reports an unusable address as
/// `isvalid: false` with no other fields, rather than as an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateAddressResponse {
    /// Whether the address is a transparent address on the queried network.
    pub isvalid: bool,
    /// The address as supplied, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Whether the address is pay-to-script-hash, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isscript: Option<bool>,
}

/// The `z_validateaddress` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZValidateAddressResponse {
    /// Whether the address is one Zaino classifies on the queried network.
    pub isvalid: bool,
    /// Whether the address belongs to the node's wallet. Zaino serves no wallet,
    /// so it is always `false` when present; emitted on a valid address, as
    /// zcashd and zebra do, and omitted on an invalid one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ismine: Option<bool>,
    /// The address, re-encoded for the queried network, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// zcashd's address-kind tag: `p2pkh`, `p2sh`, `sapling` or `unified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_type: Option<String>,
    /// The same address-kind tag under zcashd's original `type` key. Emitted
    /// alongside `address_type` because the explorer's search pattern-matches on
    /// `type`; carries the identical value.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Sapling diversifier as hex, for a Sapling address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diversifier: Option<String>,
    /// Sapling `pk_d` as hex, for a Sapling address.
    #[serde(
        rename = "diversifiedtransmissionkey",
        skip_serializing_if = "Option::is_none"
    )]
    pub diversified_transmission_key: Option<String>,
}

/// The `z_listunifiedreceivers` response: one field per receiver kind the
/// unified address bundles, absent when it carries none of that kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnifiedReceiversResponse {
    /// Orchard receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchard: Option<String>,
    /// Sapling receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sapling: Option<String>,
    /// Transparent pay-to-public-key-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2pkh: Option<String>,
    /// Transparent pay-to-script-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2sh: Option<String>,
}

/// One shielded pool's commitment-tree state in a `z_gettreestate` response,
/// nested under the pool key as `{commitments: {...}}`, matching zcashd/zebra.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolTreestateResponse {
    /// The pool's commitment-tree root and serialized state.
    pub commitments: CommitmentsResponse,
}

/// The commitment-tree root and serialized state of one pool.
///
/// `finalRoot` is the tree root after the block, in display (byte-reversed)
/// order; `finalState` is the serialized note-commitment tree as hex, in its
/// natural order. `finalRoot` is absent when the source does not report one —
/// the RPC backend discards it (see [`crate::wire::treestate_to_wire`]), so it is
/// `Option` rather than always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitmentsResponse {
    /// Root of the pool's note-commitment tree after this block, as hex (display
    /// order). Absent when the source does not report it.
    #[serde(rename = "finalRoot", skip_serializing_if = "Option::is_none")]
    pub final_root: Option<String>,
    /// The pool's serialized note-commitment tree, as hex.
    #[serde(rename = "finalState")]
    pub final_state: String,
}

/// The `z_gettreestate` response, in zcashd/zebra's shape.
///
/// A pool key (`sapling`, `orchard`, `ironwood`) is present only when the pool
/// is active at this block — an inactive pool omits the key rather than
/// rendering an empty tree, matching the source's `Option` per pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreestateResponse {
    /// Hash of the block these trees are the state after, as hex (display order).
    pub hash: String,
    /// Height of that block.
    pub height: u32,
    /// Block time, in seconds since the Unix epoch.
    pub time: u32,
    /// Sapling commitment tree; absent before Sapling activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sapling: Option<PoolTreestateResponse>,
    /// Orchard commitment tree; absent before Orchard activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchard: Option<PoolTreestateResponse>,
    /// Ironwood commitment tree; absent before NU6.3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ironwood: Option<PoolTreestateResponse>,
}

/// One entry of a `z_getsubtreesbyindex` response's `subtrees` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubtreeRootEntry {
    /// The subtree's root hash, as hex (natural order, as the source reports it).
    pub root: String,
    /// The block height at which the subtree completed.
    pub end_height: u32,
}

/// The `z_getsubtreesbyindex` response: the pool, the starting index of the run
/// actually returned, and the contiguous subtree roots from there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubtreeRootsResponse {
    /// The pool queried — `sapling` or `orchard`.
    pub pool: String,
    /// Index of the first subtree in [`Self::subtrees`].
    pub start_index: u16,
    /// The roots, in ascending index order from [`Self::start_index`]. Empty when
    /// the start index is past the end of the pool's completed subtrees.
    pub subtrees: Vec<SubtreeRootEntry>,
}

/// The `getblockheader` response, in zcashd's verbose (`verbose = true`) shape.
///
/// Hashes, the merkle root, the nonce and the Equihash solution are hex; `bits`
/// is the 8-digit hex nBits; `chainwork` is 64-character big-endian hex. The
/// optional hashes and roots are absent rather than `null` when the source does
/// not report them (genesis has no `previousblockhash`; the tip has no
/// `nextblockhash`; pre-Sapling blocks have no `finalsaplingroot`; Zebra reports
/// no `chainwork`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockHeaderResponse {
    /// Hash of this block, as hex.
    pub hash: String,
    /// Depth in the best chain, or `-1` off it. Signed for that reason.
    pub confirmations: i64,
    /// Height of this block.
    pub height: u32,
    /// Header version.
    pub version: u32,
    /// Merkle root of the transaction tree, as hex.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,
    /// Sapling commitment tree root after this block, as hex. Absent before
    /// Sapling activation and from validators that omit it.
    #[serde(rename = "finalsaplingroot", skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<String>,
    /// Block time, in seconds since the Unix epoch.
    pub time: u32,
    /// Header nonce, as hex.
    pub nonce: String,
    /// Equihash solution, as hex.
    pub solution: String,
    /// Difficulty threshold in compact (nBits) form, as 8-digit hex.
    pub bits: String,
    /// Difficulty as a multiple of the network minimum.
    pub difficulty: f64,
    /// Cumulative chainwork, as 64-character big-endian hex. Absent when the
    /// validator does not track it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chainwork: Option<String>,
    /// Hash of the previous block, as hex. Absent for genesis.
    #[serde(rename = "previousblockhash", skip_serializing_if = "Option::is_none")]
    pub previous_block_hash: Option<String>,
    /// Hash of the next block on the best chain, as hex. Absent for the tip or a
    /// side-chain block.
    #[serde(rename = "nextblockhash", skip_serializing_if = "Option::is_none")]
    pub next_block_hash: Option<String>,
}

/// One input in a verbose transaction's `vin`: either the block's coinbase
/// input, or the spend of a previous transparent output resolved to the value
/// and address it spends.
///
/// The two shapes are what the explorer reads differently: a coinbase input is a
/// script with no prevout, so it carries `coinbase` (the scriptSig hex zcashd
/// reports — a *string*, which the explorer decodes) and `sequence`. A spend
/// carries the prevout reference (`txid`, `vout`) plus the spent output's value
/// (both as a ZEC float and exact zatoshis) and the address it paid, resolved
/// through [`crate::wire`]'s prevout lookup.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TransactionInput {
    /// The block's coinbase input — present once, on the coinbase transaction.
    Coinbase {
        /// The coinbase scriptSig, as hex. A string, not a flag: the explorer
        /// decodes it (block height, miner tag).
        coinbase: String,
        /// The input's sequence number.
        sequence: u32,
    },
    /// A spend of a previous transparent output, resolved to that output.
    Spend {
        /// The spent transaction's id, as hex.
        txid: String,
        /// The spent output's index within that transaction.
        vout: u32,
        /// The input's signature script.
        #[serde(rename = "scriptSig")]
        script_sig: ScriptSig,
        /// The input's sequence number.
        sequence: u32,
        /// The spent output's value, as a ZEC-denominated float — the key the
        /// explorer sums for a transaction's input total.
        value: f64,
        /// The spent output's value, in zatoshis — the exact amount beside the
        /// float.
        #[serde(rename = "valueSat")]
        value_sat: u64,
        /// The address the spent output paid, when its script is a standard
        /// P2PKH/P2SH template; absent otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
}

/// A transparent input's signature script, as the explorer reads it: the
/// disassembled `asm` and the raw `hex`, matching zcashd's `scriptSig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptSig {
    /// The signature script disassembled to zcashd's `asm` string, with DER
    /// signatures' sighash types decoded.
    pub asm: String,
    /// The signature script, as hex.
    pub hex: String,
}

/// One output in a verbose transaction's `vout`.
///
/// Each amount is emitted twice — `value` as the ZEC float the explorer sums and
/// `valueZat` as the exact zatoshis beside it — mirroring the chain-info value
/// pools.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransactionOutput {
    /// Output value, as a ZEC-denominated float.
    pub value: f64,
    /// Output value, in zatoshis — the exact amount.
    #[serde(rename = "valueZat")]
    pub value_zat: u64,
    /// Index of this output within the transaction.
    pub n: u32,
    /// The output's locking script and the address it pays.
    #[serde(rename = "scriptPubKey")]
    pub script_pub_key: ScriptPubKey,
}

/// A transparent output's locking script, as the explorer reads it.
///
/// `addresses` and `type` are present together exactly when the script is a
/// standard P2PKH/P2SH template, and absent together otherwise — a non-standard
/// script is not an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptPubKey {
    /// The locking script disassembled to zcashd's `asm` string.
    pub asm: String,
    /// The locking script, as hex.
    pub hex: String,
    /// Number of signatures required to spend — `1` for a standard P2PKH/P2SH
    /// template; absent for a non-standard script, matching zcashd.
    #[serde(rename = "reqSigs", skip_serializing_if = "Option::is_none")]
    pub required_signatures: Option<u32>,
    /// The address the output pays, as a one-element array; absent when the
    /// script is non-standard. The explorer iterates it, so a non-standard
    /// output omits the key rather than rendering an empty or null array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<String>>,
    /// zcashd's script-type tag — `pubkeyhash` or `scripthash`; absent for a
    /// non-standard script.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub script_type: Option<String>,
}

/// One Sprout JoinSplit's transparent value movement, each amount as both the
/// ZEC float the explorer sums and the exact zatoshis beside it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JoinSplitObject {
    /// Value removed from the transparent pool, as a ZEC float.
    pub vpub_old: f64,
    /// Value removed from the transparent pool, in zatoshis.
    #[serde(rename = "vpub_oldZat")]
    pub vpub_old_zat: u64,
    /// Value inserted into the transparent pool, as a ZEC float.
    pub vpub_new: f64,
    /// Value inserted into the transparent pool, in zatoshis.
    #[serde(rename = "vpub_newZat")]
    pub vpub_new_zat: u64,
}

/// One Sapling spend in `vShieldedSpend`. The explorer reads only the array's
/// length, so a single field (the nullifier, in zcashd's spelling) carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShieldedSpend {
    /// The spend's nullifier, as hex.
    pub nullifier: String,
}

/// One Sapling output in `vShieldedOutput`. The explorer reads only the array's
/// length; the fields are zcashd's spellings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShieldedOutput {
    /// Note commitment (`cmu`), as hex.
    pub cmu: String,
    /// Ephemeral key for recipient detection, as hex.
    #[serde(rename = "ephemeralKey")]
    pub ephemeral_key: String,
}

/// A version-5 transaction's Orchard bundle, as the explorer reads it on the
/// transaction page.
///
/// Emitted on every version-5 transaction, including one with no actions — the
/// explorer dereferences `orchard.valueBalance` strictly on v5, so an absent
/// `orchard` crashes it (Review Focus 5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrchardObject {
    /// The Orchard actions. The explorer reads only the length.
    pub actions: Vec<OrchardActionObject>,
    /// Net Orchard value balance, as a ZEC float.
    #[serde(rename = "valueBalance")]
    pub value_balance: f64,
    /// Net Orchard value balance, in zatoshis — signed, the exact amount.
    #[serde(rename = "valueBalanceZat")]
    pub value_balance_zat: i64,
}

/// One Orchard action. The explorer reads only `length(orchard.actions)`; the
/// fields are zcashd's spellings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrchardActionObject {
    /// Nullifier, as hex.
    pub nullifier: String,
    /// Note commitment (`cmx`), as hex.
    pub cmx: String,
    /// Ephemeral key for recipient detection, as hex.
    #[serde(rename = "ephemeralKey")]
    pub ephemeral_key: String,
}

/// One transaction, in the shape the explorer parses for both
/// `getrawtransaction <txid> 1` and each element of `getblock <block> 2`'s `tx`.
///
/// The conditional keys follow the transaction's envelope: `versiongroupid` and
/// `expiryheight` appear on an overwintered transaction; `valueBalance`,
/// `valueBalanceZat`, `vShieldedSpend` and `vShieldedOutput` from version 4;
/// `orchard` from version 5. Ironwood is deliberately not emitted — the explorer
/// has no field for it (a recorded divergence).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransactionObject {
    /// Transaction id, as hex.
    pub txid: String,
    /// Transaction version number.
    pub version: u32,
    /// Whether the Overwinter format flag is set.
    pub overwintered: bool,
    /// Version group id, as 8-digit hex; present only when overwintered.
    #[serde(rename = "versiongroupid", skip_serializing_if = "Option::is_none")]
    pub version_group_id: Option<String>,
    /// Raw `nLockTime`.
    pub locktime: u32,
    /// Expiry height; present only when overwintered.
    #[serde(rename = "expiryheight", skip_serializing_if = "Option::is_none")]
    pub expiry_height: Option<u32>,
    /// Serialized byte length of the transaction.
    pub size: u64,
    /// The transaction's raw consensus bytes, as hex.
    pub hex: String,
    /// Inputs — one coinbase input on the coinbase transaction, spends
    /// otherwise.
    pub vin: Vec<TransactionInput>,
    /// Outputs, in order.
    pub vout: Vec<TransactionOutput>,
    /// Sprout JoinSplits, in order; empty on a non-Sprout transaction.
    pub vjoinsplit: Vec<JoinSplitObject>,
    /// Net Sapling value balance, as a ZEC float; present from version 4.
    #[serde(rename = "valueBalance", skip_serializing_if = "Option::is_none")]
    pub value_balance: Option<f64>,
    /// Net Sapling value balance, in zatoshis; present from version 4.
    #[serde(rename = "valueBalanceZat", skip_serializing_if = "Option::is_none")]
    pub value_balance_zat: Option<i64>,
    /// Sapling spends; present from version 4.
    #[serde(rename = "vShieldedSpend", skip_serializing_if = "Option::is_none")]
    pub shielded_spends: Option<Vec<ShieldedSpend>>,
    /// Sapling outputs; present from version 4.
    #[serde(rename = "vShieldedOutput", skip_serializing_if = "Option::is_none")]
    pub shielded_outputs: Option<Vec<ShieldedOutput>>,
    /// Orchard bundle; emitted on every transaction, with empty actions and a
    /// zero value balance when the transaction has no Orchard bundle — matching
    /// zebra, which renders it even for version-4 transactions.
    pub orchard: OrchardObject,
    /// Whether the transaction is in the active (best) chain. Present when
    /// derivable — `true` for a transaction in a best-chain block, `false` for a
    /// side-chain one; absent for a mempool transaction, which is in no chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_active_chain: Option<bool>,
}

/// The `getrawtransaction <txid> 1` response: the shared [`TransactionObject`]
/// plus where the transaction sits in the chain.
///
/// The location keys are flattened onto the transaction object, matching zcashd,
/// and are all absent for a mempool transaction, which has no containing block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RawTransactionResponse {
    /// The transaction, in the shared explorer shape.
    #[serde(flatten)]
    pub transaction: TransactionObject,
    /// Height of the containing block; absent for a mempool transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Depth of the containing block in the best chain; absent for a mempool
    /// transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<i64>,
    /// Hash of the containing block, as hex; absent for a mempool transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blockhash: Option<String>,
    /// Time of the containing block, in Unix seconds; absent for a mempool
    /// transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<u32>,
    /// Time of the containing block, in Unix seconds (zcashd emits it twice);
    /// absent for a mempool transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocktime: Option<u32>,
}

/// One pool's cumulative note-commitment tree size, as the `trees` object nests
/// it: a single-key `{size}` object per pool, matching zcashd/zebra.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreePoolSize {
    /// Cumulative note count in this pool's commitment tree after the block.
    pub size: u64,
}

/// The `trees` object of a verbose `getblock`: the cumulative note-commitment
/// tree size of each shielded pool after this block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreesResponse {
    /// Sapling pool tree size.
    pub sapling: TreePoolSize,
    /// Orchard pool tree size.
    pub orchard: TreePoolSize,
    /// Ironwood pool tree size (NU6.3).
    pub ironwood: TreePoolSize,
}

/// The `getblock` response at verbosity 1 or 2. The two verbosities share every
/// field but `tx`: verbosity 1 lists the transaction ids as strings, verbosity 2
/// the decoded [`TransactionObject`]s — so `T` is `String` or
/// [`TransactionObject`].
///
/// `size` is the serialized block length, from the decoded-block read (which
/// closes the earlier divergence where it could not be measured).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockResponse<T> {
    /// Hash of this block, as hex.
    pub hash: String,
    /// Depth in the best chain, or `-1` off it.
    pub confirmations: i64,
    /// Height of this block.
    pub height: u32,
    /// Header version.
    pub version: u32,
    /// Merkle root of the transaction tree, as hex.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,
    /// Block commitments digest, as hex (natural order).
    #[serde(rename = "blockcommitments")]
    pub block_commitments: String,
    /// Sapling commitment tree root after this block, as hex. Absent before
    /// Sapling activation and from validators that omit it.
    #[serde(rename = "finalsaplingroot", skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<String>,
    /// Orchard commitment tree root after this block, as hex. Absent before
    /// Orchard activation and from validators that omit it.
    #[serde(rename = "finalorchardroot", skip_serializing_if = "Option::is_none")]
    pub final_orchard_root: Option<String>,
    /// Number of transactions in the block.
    #[serde(rename = "nTx")]
    pub n_tx: u32,
    /// Block time, in seconds since the Unix epoch.
    pub time: u32,
    /// Header nonce, as hex.
    pub nonce: String,
    /// Equihash solution, as hex.
    pub solution: String,
    /// Difficulty threshold in compact (nBits) form, as 8-digit hex.
    pub bits: String,
    /// Difficulty as a multiple of the network minimum.
    pub difficulty: f64,
    /// Cumulative chainwork, as 64-character big-endian hex. Absent when the
    /// validator does not track it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chainwork: Option<String>,
    /// Total chain value as of this block, unnamed. Absent when the validator
    /// does not report per-block supply.
    #[serde(rename = "chainSupply", skip_serializing_if = "Option::is_none")]
    pub chain_supply: Option<ValuePoolResponse>,
    /// Per-pool value balances as of this block. Empty when the validator reports
    /// none.
    #[serde(rename = "valuePools")]
    pub value_pools: Vec<ValuePoolResponse>,
    /// Cumulative note-commitment tree sizes of each shielded pool after this
    /// block.
    pub trees: TreesResponse,
    /// Serialized byte length of the whole block.
    pub size: u64,
    /// Hash of the previous block, as hex. Absent for genesis.
    #[serde(rename = "previousblockhash", skip_serializing_if = "Option::is_none")]
    pub previous_block_hash: Option<String>,
    /// Hash of the next block on the best chain, as hex. Absent for the tip.
    #[serde(rename = "nextblockhash", skip_serializing_if = "Option::is_none")]
    pub next_block_hash: Option<String>,
    /// The block's transactions — ids at verbosity 1, decoded at verbosity 2.
    pub tx: Vec<T>,
}

/// The `getrawtransaction` response across the served verbosities. Serialized
/// untagged, so each variant is its own JSON: verbosity 0 a bare hex string,
/// verbosity 1 the decoded object. A single jsonrpsee method returns one type,
/// and zcashd's `getrawtransaction` is polymorphic by verbosity, so the enum is
/// that one type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum GetRawTransactionResponse {
    /// Verbosity 0: the raw consensus bytes as lowercase hex.
    Raw(String),
    /// Verbosity 1: the decoded transaction with its chain location.
    Verbose(Box<RawTransactionResponse>),
}

/// The `getblock` response across the served verbosities. Serialized untagged:
/// verbosity 1 carries `tx` as id strings, verbosity 2 as decoded transactions.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum GetBlockResponse {
    /// Verbosity 0: the raw consensus bytes as lowercase hex.
    Raw(String),
    /// Verbosity 1: `tx` is the list of transaction ids.
    Verbose1(BlockResponse<String>),
    /// Verbosity 2: `tx` is the list of decoded transactions.
    Verbose2(BlockResponse<TransactionObject>),
}

/// One entry of the `getblockhashes` verbose response, emitted when the request
/// sets `logicalTimes: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockHashLogical {
    /// The block hash, in RPC display order.
    pub blockhash: String,
    /// The block's logical timestamp (its `nTime`).
    pub logicalts: u32,
}

/// The `getblockhashes` response. Serialized untagged, so each variant is its own
/// JSON: a bare array of hash strings when `logicalTimes` is false or absent, an
/// array of `{blockhash, logicalts}` objects when it is true. One jsonrpsee method
/// returns one type, and zcashd's `getblockhashes` is polymorphic by that option,
/// so the enum is that one type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum GetBlockHashesResponse {
    /// `logicalTimes` false or absent: the block hashes, in display order.
    Hashes(Vec<String>),
    /// `logicalTimes: true`: each hash with its logical timestamp.
    Logical(Vec<BlockHashLogical>),
}

/// One value pool in a `getblockchaininfo` response.
///
/// zcashd reports each amount twice — once as a ZEC-denominated float
/// (`chainValue`) and once in zatoshis (`chainValueZat`). Both are emitted: the
/// float is the key clients pattern-match on, and the exact integer travels
/// beside it for callers that need it. The float is lossy by nature; the integer
/// is authoritative.
///
/// The same shape serves the unnamed chain-supply total and the named pools: a
/// pool has an `id`, the total has none, so an empty id is omitted rather than
/// rendered as `""`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValuePoolResponse {
    /// Pool name — `transparent`, `sapling`, `orchard`, … Absent for the
    /// chain-supply total, which zcashd reports unnamed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Whether the validator is tracking this pool's balance.
    pub monitored: bool,
    /// Total value currently in the pool, as a ZEC-denominated float.
    #[serde(rename = "chainValue")]
    pub chain_value: f64,
    /// Total value currently in the pool, in zatoshis — the exact amount.
    #[serde(rename = "chainValueZat")]
    pub chain_value_zat: u64,
    /// Change to the pool's balance from the latest block, as a ZEC float. Absent
    /// when the validator does not report a delta.
    #[serde(rename = "valueDelta", skip_serializing_if = "Option::is_none")]
    pub value_delta: Option<f64>,
    /// Change to the pool's balance from the latest block, in zatoshis — the
    /// exact amount. Absent when the validator does not report a delta; signed,
    /// as value leaves a pool as well as entering it.
    #[serde(rename = "valueDeltaZat", skip_serializing_if = "Option::is_none")]
    pub value_delta_zat: Option<i64>,
}

/// One network upgrade in a `getblockchaininfo` response, keyed in the enclosing
/// map by its consensus branch id (zcashd's layout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkUpgradeResponse {
    /// The validator's descriptive name for the upgrade, e.g. `Canopy`, `NU5`.
    pub name: String,
    /// Height at which the upgrade activates.
    #[serde(rename = "activationheight")]
    pub activation_height: u32,
    /// Status at the validator's current tip: `active`, `pending` or `disabled`.
    pub status: String,
}

/// The consensus branches in force around the tip, as `getblockchaininfo`
/// reports them: 8-digit lowercase hex branch ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TipConsensusResponse {
    /// Branch in force at the current tip.
    pub chaintip: String,
    /// Branch that will be in force for the next block.
    pub nextblock: String,
}

/// The `getblockchaininfo` response, in zcashd's field names and encodings.
///
/// `chainwork` is absent rather than zero when the validator does not track
/// cumulative work (zebra hardcodes it): zero is not a possible amount of work,
/// so the honest wire form is omission.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockchainInfoResponse {
    /// Network name as defined in BIP70 — `main`, `test`, `regtest`.
    pub chain: String,
    /// Number of blocks the validator has fully processed.
    pub blocks: u32,
    /// Height of the best header chain the validator has validated.
    pub headers: u32,
    /// Hash of the current best block, as hex.
    #[serde(rename = "bestblockhash")]
    pub best_block_hash: String,
    /// Current difficulty, as a multiple of the network minimum.
    pub difficulty: f64,
    /// Verification progress relative to the estimated network tip, in `0.0..=1.0`.
    #[serde(rename = "verificationprogress")]
    pub verification_progress: f64,
    /// Total work in the best chain, as 64-character big-endian hex. Absent when
    /// the validator does not track it.
    #[serde(rename = "chainwork", skip_serializing_if = "Option::is_none")]
    pub chain_work: Option<String>,
    /// Whether the validator has pruned block data.
    pub pruned: bool,
    /// Approximate on-disk size of the validator's block and undo data, in bytes.
    pub size_on_disk: u64,
    /// Total note commitments across the shielded pools.
    pub commitments: u64,
    /// Height the validator estimates the network tip to be at.
    #[serde(rename = "estimatedheight")]
    pub estimated_height: u32,
    /// Total transparent and shielded value on the chain, unnamed.
    #[serde(rename = "chainSupply")]
    pub chain_supply: ValuePoolResponse,
    /// Per-pool value balances.
    #[serde(rename = "valuePools")]
    pub value_pools: Vec<ValuePoolResponse>,
    /// Network upgrade schedule, keyed by 8-digit lowercase hex branch id.
    pub upgrades: BTreeMap<String, NetworkUpgradeResponse>,
    /// Consensus branches in force at the tip and for the next block.
    pub consensus: TipConsensusResponse,
}

/// The `getinfo` response.
///
/// `build` is load-bearing for the explorer, which pattern-matches it as a
/// string on its homepage; it is always present and never optional.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeInfoResponse {
    /// Validator version, as its own numeric encoding.
    pub version: u64,
    /// Validator build identifier.
    pub build: String,
    /// Network protocol user-agent string.
    pub subversion: String,
    /// Peer-to-peer protocol version.
    pub protocolversion: u32,
    /// Height of the validator's best chain.
    pub blocks: u32,
    /// Total peer connections, inbound and outbound.
    pub connections: u64,
    /// Current difficulty as a multiple of the network minimum.
    pub difficulty: f64,
    /// Whether the validator considers itself on a test network.
    pub testnet: bool,
    /// Configured proxy, when the validator reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Minimum transaction fee, in zatoshis per kilobyte.
    pub paytxfee: u64,
    /// Minimum relay fee, in zatoshis per kilobyte.
    pub relayfee: u64,
    /// The validator's last error or warning, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
    /// When `errors` was raised, in seconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errorstimestamp: Option<i64>,
}

/// The `getmininginfo` response, in zcashd's wire field names.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MiningInfoResponse {
    /// Height of the current best-chain tip.
    pub blocks: u32,
    /// Size in bytes of the last block the validator built, when it tracks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currentblocksize: Option<u64>,
    /// Transaction count in the last block the validator built, when it tracks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currentblocktx: Option<u64>,
    /// Current difficulty as a multiple of the network minimum, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<f64>,
    /// Estimated network solution rate, in solutions per second, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networksolps: Option<u64>,
    /// Estimated network hash rate, in hashes per second, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networkhashps: Option<u64>,
    /// Name of the chain being served.
    pub chain: String,
    /// Whether the validator considers itself on a test network.
    pub testnet: bool,
    /// Validator status or error message, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
}

/// One entry of the `getpeerinfo` list.
///
/// The explorer reads `addr` tolerantly and nothing else, but `inbound` is the
/// only other fact the supported validators report, so both travel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerInfoEntry {
    /// Remote peer address as the validator reports it.
    pub addr: String,
    /// Whether the peer initiated the connection to the validator.
    pub inbound: bool,
}

#[cfg(test)]
mod tests {
    use super::{
        AddressBalanceResponse, AddressDeltaEntry, AddressDeltasResponse, DeltaRange,
        UnifiedReceiversResponse, ValidateAddressResponse,
    };
    use serde_json::Value;

    /// The sorted key set of a JSON object, for pinning the exact wire shape:
    /// an extra key fails the comparison as surely as a missing one.
    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("response serializes to a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn to_value<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("response serializes")
    }

    /// `getaddressbalance` carries exactly `balance` and `received`, both as JSON
    /// numbers. Fails if a key is renamed/added/dropped or a quantity is rendered
    /// as a string instead of a number.
    #[test]
    fn address_balance_response_golden_shape() {
        let json = to_value(&AddressBalanceResponse {
            balance: 5_000,
            received: 12_000,
        });
        assert_eq!(sorted_keys(&json), ["balance", "received"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("balance").and_then(Value::as_u64), Some(5_000));
        assert_eq!(obj.get("received").and_then(Value::as_u64), Some(12_000));
    }

    /// One delta entry with a known block index carries all six keys, with
    /// `block_index` rendered under the zcashd spelling `blockindex`. Fails if a
    /// key is renamed/added/dropped or a number is rendered as a string.
    #[test]
    fn address_delta_entry_with_block_index_golden_shape() {
        let json = to_value(&AddressDeltaEntry {
            satoshis: -3,
            txid: "aa".repeat(32),
            index: 2,
            block_index: Some(1),
            height: 150,
            address: "t1a".to_string(),
        });
        assert_eq!(
            sorted_keys(&json),
            [
                "address",
                "blockindex",
                "height",
                "index",
                "satoshis",
                "txid"
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("satoshis").and_then(Value::as_i64), Some(-3));
        assert_eq!(
            obj.get("txid").and_then(Value::as_str),
            Some("aa".repeat(32).as_str())
        );
        assert_eq!(obj.get("index").and_then(Value::as_u64), Some(2));
        assert_eq!(obj.get("blockindex").and_then(Value::as_u64), Some(1));
        assert_eq!(obj.get("height").and_then(Value::as_u64), Some(150));
        assert_eq!(obj.get("address").and_then(Value::as_str), Some("t1a"));
    }

    /// Documented divergence from zcashd, pinned to *our* shape: zcashd always
    /// emits `blockindex`, whereas Zaino omits it when the source cannot supply
    /// the in-block position. Recorded in the spec
    /// (`zaino-design/design/explorer-grant-completion.md`, "Known wire
    /// divergences"). Do not "fix" this to match zcashd — the omission is the
    /// honest answer when the position is unknown. `blockindex` must be *absent*,
    /// not `null`, so `skip_serializing_if` is pinned here.
    #[test]
    fn address_delta_entry_omits_block_index_when_absent() {
        let json = to_value(&AddressDeltaEntry {
            satoshis: 7,
            txid: "bb".repeat(32),
            index: 0,
            block_index: None,
            height: 10,
            address: "t1b".to_string(),
        });
        assert_eq!(
            sorted_keys(&json),
            ["address", "height", "index", "satoshis", "txid"]
        );
        let obj = json.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("blockindex"),
            "an absent block index is omitted, not rendered as null"
        );
    }

    /// Documented divergence from zcashd, pinned to *our* shape: zcashd returns
    /// top-level `start` and `end` objects each `{hash, height}`, whereas Zaino
    /// wraps one `range` object of bare heights. Recorded in the spec
    /// (`zaino-design/design/explorer-grant-completion.md`, "Known wire
    /// divergences"). Do not "fix" this test to zcashd's shape — the wrapper is
    /// intentional.
    #[test]
    fn address_deltas_response_with_range_golden_shape() {
        let json = to_value(&AddressDeltasResponse {
            deltas: vec![AddressDeltaEntry {
                satoshis: -3,
                txid: "aa".repeat(32),
                index: 0,
                block_index: Some(1),
                height: 150,
                address: "t1a".to_string(),
            }],
            range: Some(DeltaRange {
                start: 100,
                end: 200,
            }),
        });
        assert_eq!(sorted_keys(&json), ["deltas", "range"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("deltas").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
        let range = obj.get("range").expect("range present");
        assert_eq!(sorted_keys(range), ["end", "start"]);
        let range = range.as_object().expect("range is an object");
        assert_eq!(range.get("start").and_then(Value::as_u64), Some(100));
        assert_eq!(range.get("end").and_then(Value::as_u64), Some(200));
    }

    /// Without `chainInfo`, the response is the bare delta list — `range` is
    /// absent, not `null`. Pins the `skip_serializing_if` on `range`.
    #[test]
    fn address_deltas_response_without_range_omits_it() {
        let json = to_value(&AddressDeltasResponse {
            deltas: Vec::new(),
            range: None,
        });
        assert_eq!(sorted_keys(&json), ["deltas"]);
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("range"),
            "an absent range is omitted, not rendered as null"
        );
    }

    /// A valid transparent address carries `isvalid`, `address` and `isscript`.
    /// Fails if a key is renamed/added/dropped or a type changes.
    #[test]
    fn validate_address_response_valid_golden_shape() {
        let json = to_value(&ValidateAddressResponse {
            isvalid: true,
            address: Some("t1abc".to_string()),
            isscript: Some(false),
        });
        assert_eq!(sorted_keys(&json), ["address", "isscript", "isvalid"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(Value::as_bool), Some(true));
        assert_eq!(obj.get("address").and_then(Value::as_str), Some("t1abc"));
        assert_eq!(obj.get("isscript").and_then(Value::as_bool), Some(false));
    }

    /// An invalid address reports only `isvalid: false`; the two optional keys
    /// are absent, not `null`. Pins their `skip_serializing_if`.
    #[test]
    fn validate_address_response_invalid_golden_shape() {
        let json = to_value(&ValidateAddressResponse {
            isvalid: false,
            address: None,
            isscript: None,
        });
        assert_eq!(sorted_keys(&json), ["isvalid"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(Value::as_bool), Some(false));
        assert!(!obj.contains_key("address"));
        assert!(!obj.contains_key("isscript"));
    }

    /// A unified address bundling every receiver kind renders all four keys.
    /// Fails if a key is renamed/added/dropped or a value's type changes.
    #[test]
    fn unified_receivers_response_all_present_golden_shape() {
        let json = to_value(&UnifiedReceiversResponse {
            orchard: Some("u1orchard".to_string()),
            sapling: Some("zs1sapling".to_string()),
            p2pkh: Some("t1pkh".to_string()),
            p2sh: Some("t3sh".to_string()),
        });
        assert_eq!(sorted_keys(&json), ["orchard", "p2pkh", "p2sh", "sapling"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("orchard").and_then(Value::as_str),
            Some("u1orchard")
        );
        assert_eq!(
            obj.get("sapling").and_then(Value::as_str),
            Some("zs1sapling")
        );
        assert_eq!(obj.get("p2pkh").and_then(Value::as_str), Some("t1pkh"));
        assert_eq!(obj.get("p2sh").and_then(Value::as_str), Some("t3sh"));
    }

    /// Receiver kinds the address does not carry are omitted, not `null`. Pins
    /// the per-field `skip_serializing_if`.
    #[test]
    fn unified_receivers_response_partly_absent_golden_shape() {
        let json = to_value(&UnifiedReceiversResponse {
            orchard: Some("u1orchard".to_string()),
            sapling: None,
            p2pkh: Some("t1pkh".to_string()),
            p2sh: None,
        });
        assert_eq!(sorted_keys(&json), ["orchard", "p2pkh"]);
        let obj = json.as_object().expect("a JSON object");
        assert!(!obj.contains_key("sapling"));
        assert!(!obj.contains_key("p2sh"));
    }
}
