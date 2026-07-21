//! Published wallet-API vocabulary.
//!
//! Bytes payloads + id newtypes (ADR-0002). This is the churn-insulated public
//! surface: **no `zaino-core` / domain primitive ever crosses it**, so domain
//! types stay free to churn. These are the crate's own types on purpose.

/// A block's height + hash, published form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId {
    pub height: u32,
    pub hash: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TxId(pub [u8; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Outpoint {
    pub txid: TxId,
    pub index: u32,
}

/// Consensus-serialised block bytes — the consumer parses into its own types.
#[derive(Clone, Debug)]
pub struct RawBlock(pub Vec<u8>);

/// Consensus-serialised transaction bytes.
#[derive(Clone, Debug)]
pub struct RawTransaction(pub Vec<u8>);

/// Consensus-serialised treestate/frontier bytes.
#[derive(Clone, Debug)]
pub struct RawTreestate(pub Vec<u8>);

/// A subtree root, published form (opaque bytes for the scaffold).
#[derive(Clone, Debug)]
pub struct RawSubtreeRoot(pub Vec<u8>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    Sapling,
    Orchard,
    Ironwood,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendStatus {
    Unspent,
    Spent(TxId),
    SpentSpenderUnknown,
    NoSuchOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Mined(u32),
    Orphaned,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeStatus {
    Active,
    Pending,
    Disabled,
}

#[derive(Clone, Debug)]
pub struct ReportedUpgrade {
    pub branch_id: u32,
    pub name: String,
    pub activation_height: u32,
    pub status: UpgradeStatus,
}

/// Errors surfaced to the consumer. (Scaffold: coarse; will map from the
/// inner per-capability errors.)
#[derive(Debug)]
pub enum WalletError {
    NotServiceable,
    Transient(String),
    Fatal(String),
    Rejected(String),
}
