//! The explorer's domain: a block explorer's one driven port, reading chain
//! state. The TUI and the web surface (see `zaino-explorer-web`,
//! `zaino-explorer-tui`) are two driving adapters over this same domain;
//! `zaino-explorer-zaino-client` is its one production implementor, talking
//! to a live zainod. Nothing in this crate depends on jsonrpsee, zaino-noderpc,
//! or any other transport detail — that coupling lives only in an adapter
//! crate, injected at each driving adapter's composition root.
#![forbid(unsafe_code)]

use std::future::Future;

/// Reads chain state. The explorer's one driven port.
pub trait ChainReader: Clone + Send + Sync + 'static {
    /// The current chain height.
    fn chain_height(&self) -> impl Future<Output = Result<u32, ChainReadError>> + Send;

    /// The `count` most recent blocks, newest first.
    fn recent_blocks(
        &self,
        count: u32,
    ) -> impl Future<Output = Result<Vec<BlockSummary>, ChainReadError>> + Send;

    /// One block's full detail — including every transaction id it
    /// contains — by height or hash. Lets a block list navigate down into
    /// its transactions, the same way `address`'s `txids` does for an
    /// address's history.
    fn block(
        &self,
        height_or_hash: String,
    ) -> impl Future<Output = Result<BlockDetail, ChainReadError>> + Send;

    /// A block's per-transaction transparent value movements, by hash. A
    /// differentiator capability: Zebra itself does not serve
    /// `getblockdeltas`, so no explorer built directly on Zebra's own RPC
    /// can offer this.
    fn block_deltas(
        &self,
        hash: String,
    ) -> impl Future<Output = Result<BlockDeltas, ChainReadError>> + Send;

    /// A block's shielded commitment-tree state (`z_gettreestate`), by
    /// height or hash — each active pool's tree root and serialized state.
    fn treestate(
        &self,
        height_or_hash: String,
    ) -> impl Future<Output = Result<Treestate, ChainReadError>> + Send;

    /// One transaction, by id.
    fn transaction(
        &self,
        txid: String,
    ) -> impl Future<Output = Result<TransactionDetail, ChainReadError>> + Send;

    /// Where a specific transparent output was spent, if at all. A
    /// differentiator capability: Zebra itself does not serve
    /// `getspentinfo`, so no explorer built directly on Zebra's own RPC can
    /// offer this.
    fn spend_info(
        &self,
        txid: String,
        output_index: u32,
    ) -> impl Future<Output = Result<SpendInfo, ChainReadError>> + Send;

    /// One transparent address's balance and transaction history.
    fn address(
        &self,
        address: String,
    ) -> impl Future<Output = Result<AddressSummary, ChainReadError>> + Send;

    /// Whether a given string is an address Zaino recognizes on this
    /// network, and its kind if so (`z_validateaddress` — a strict
    /// superset of `validateaddress`, covering shielded and unified
    /// addresses too, not just transparent).
    fn validate_address(
        &self,
        address: String,
    ) -> impl Future<Output = Result<AddressValidity, ChainReadError>> + Send;

    /// The validator's own status and the mempool's current size.
    fn node_status(&self) -> impl Future<Output = Result<NodeStatus, ChainReadError>> + Send;

    /// Every transaction currently in the mempool, with its entry detail.
    fn raw_mempool(&self)
        -> impl Future<Output = Result<Vec<MempoolEntry>, ChainReadError>> + Send;
}

/// One block's summary, as shown in a block list — not the full block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSummary {
    /// The block's height.
    pub height: u32,
    /// The block's hash, in the wire's display (byte-reversed) hex form.
    pub hash: String,
    /// The block's timestamp (seconds since the Unix epoch).
    pub time: u32,
    /// Number of transactions in the block.
    pub tx_count: u32,
}

/// One block's full detail, as shown on a block detail page or screen —
/// everything [`BlockSummary`] has, plus every transaction id the block
/// contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDetail {
    /// The block's height.
    pub height: u32,
    /// The block's hash, in the wire's display (byte-reversed) hex form.
    pub hash: String,
    /// The block's timestamp (seconds since the Unix epoch).
    pub time: u32,
    /// Every transaction id in the block, in block order.
    pub tx_ids: Vec<String>,
}

/// A block's per-transaction transparent value movements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDeltas {
    /// The block's hash.
    pub hash: String,
    /// The block's height.
    pub height: u32,
    /// Each transaction's value movements, in block order.
    pub deltas: Vec<TransactionDelta>,
}

/// One transaction's transparent value movements within a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionDelta {
    /// The transaction's id.
    pub txid: String,
    /// The transparent inputs, each a negative value movement. Empty for a
    /// coinbase transaction.
    pub inputs: Vec<ValueMovement>,
    /// The transparent outputs, each a positive value movement.
    pub outputs: Vec<ValueMovement>,
}

/// A single transparent value movement: negative for a spend, positive for
/// a receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueMovement {
    /// The address the value moved through, when the script is a standard
    /// template; absent otherwise.
    pub address: Option<String>,
    /// The signed value, in zatoshis.
    pub value_zat: i64,
}

/// A block's shielded commitment-tree state, by pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Treestate {
    /// The block's hash.
    pub hash: String,
    /// The block's height.
    pub height: u32,
    /// The block's timestamp (seconds since the Unix epoch).
    pub time: u32,
    /// The Sapling pool's tree state; absent before Sapling activation.
    pub sapling: Option<PoolTreestate>,
    /// The Orchard pool's tree state; absent before Orchard activation.
    pub orchard: Option<PoolTreestate>,
    /// The Ironwood pool's tree state; absent before NU6.3.
    pub ironwood: Option<PoolTreestate>,
}

/// One pool's commitment-tree root and serialized state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTreestate {
    /// The tree's root after this block, as hex (display order). Absent
    /// when the source does not report it.
    pub final_root: Option<String>,
    /// The pool's serialized note-commitment tree, as hex. Can be large —
    /// a renderer should summarize rather than print it in full.
    pub final_state: String,
}

/// One transaction, as shown on a transaction detail page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetail {
    /// The transaction's id.
    pub txid: String,
    /// Serialized byte length.
    pub size: u64,
    /// Height of the containing block; absent for a mempool transaction.
    pub height: Option<u32>,
    /// Depth of the containing block in the best chain; absent for a
    /// mempool transaction.
    pub confirmations: Option<i64>,
    /// This transaction's transparent outputs.
    pub outputs: Vec<TransactionOutput>,
}

/// One transparent output, as shown on a transaction detail page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionOutput {
    /// The output's value, in zatoshis.
    pub value_zat: u64,
    /// The address this output pays, when its script is a standard
    /// template. Empty for a non-standard script.
    pub addresses: Vec<String>,
}

/// Where a transparent output was spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendInfo {
    /// The id of the transaction that spent it.
    pub spending_txid: String,
    /// The spending transaction's input index.
    pub spending_input_index: u32,
    /// The height that mined the spending transaction.
    pub height: u32,
}

/// Whether an address is one Zaino recognizes, and its kind if so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressValidity {
    /// Whether the address is valid on the queried network.
    pub valid: bool,
    /// The address, re-encoded for the queried network, when valid.
    pub address: Option<String>,
    /// The address kind — `p2pkh`, `p2sh`, `sapling`, or `unified` — when
    /// valid.
    pub kind: Option<String>,
}

/// One transparent address's balance and recent transaction history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressSummary {
    /// The address itself.
    pub address: String,
    /// Total currently held, in zatoshis.
    pub balance_zat: u64,
    /// Lifetime gross receipts, in zatoshis (not supply-bounded, hence the
    /// wider type).
    pub received_zat: u128,
    /// Transaction ids this address appears in.
    pub txids: Vec<String>,
    /// The address's currently unspent transparent outputs. Empty when
    /// `getaddressutxos` fails (not every deployed zainod serves it) —
    /// degrades the same way `txids` does, rather than hiding balance and
    /// received.
    pub utxos: Vec<AddressUtxo>,
    /// Every transparent value change at this address, in `(height,
    /// blockindex, index)` order. Empty when `getaddressdeltas` fails, for
    /// the same reason.
    pub deltas: Vec<AddressDelta>,
}

/// One unspent transparent output held by an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressUtxo {
    /// The transaction containing the output.
    pub txid: String,
    /// The output's index within that transaction.
    pub output_index: u32,
    /// The output's locking script, as hex.
    pub script: String,
    /// The output's value, in zatoshis.
    pub value_zat: u64,
    /// Block height at which the output was created.
    pub height: u32,
}

/// One transparent value change at an address — negative for a spend,
/// positive for a receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDelta {
    /// The transaction that caused the change.
    pub txid: String,
    /// Input or output index within the transaction.
    pub index: u32,
    /// Block height of the change.
    pub height: u32,
    /// The signed value, in zatoshis.
    pub value_zat: i64,
}

/// The validator's own status, plus the mempool's current size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    /// Network protocol user-agent string (e.g. `/Zebra:6.4.2/`).
    pub subversion: String,
    /// Total peer connections, inbound and outbound.
    pub connections: u64,
    /// Number of transactions currently in the mempool.
    pub mempool_size: u64,
    /// Total serialized size of the mempool's transactions, in bytes.
    pub mempool_bytes: u64,
}

/// One mempool transaction's entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolEntry {
    /// The transaction's id.
    pub txid: String,
    /// Serialized byte length.
    pub size: u64,
    /// The transaction's fee, in zatoshis.
    pub fee_zat: u64,
    /// When the transaction entered the mempool, when known.
    pub time: Option<i64>,
    /// Chain tip height when the transaction entered the mempool.
    pub height: u32,
}

/// Why a [`ChainReader`] read failed.
#[derive(Debug, thiserror::Error)]
pub enum ChainReadError {
    /// The RPC call itself failed (network, decode, or an error the node
    /// returned) — the adapter's only failure mode today.
    #[error("chain read failed")]
    Rpc(#[source] Box<dyn std::error::Error + Send + Sync>),
}
