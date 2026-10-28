//! Every index Zaino folds, and each one's store declaration

use zaino_persistence::{Schema, Tables};
use zcash_protocol::consensus::NetworkType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexKind {
    CompactBlock,
    TreeState,
    TransparentAddress,
    BlockHash,
    UtxoSet,
    TxidLocation,
}

impl IndexKind {
    /// Fold order (utxo-set before compact-block: its fees)
    pub const ALL: [Self; 6] = [
        Self::UtxoSet,
        Self::CompactBlock,
        Self::BlockHash,
        Self::TreeState,
        Self::TransparentAddress,
        Self::TxidLocation,
    ];

    /// One spelling everywhere (metric label, `/status`, task, default dir, index-snapshot entry,
    /// the store's name in its MANIFEST)
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompactBlock => "compact_block",
            Self::TreeState => "tree_state",
            Self::TransparentAddress => "transparent_address",
            Self::BlockHash => "block_hash",
            Self::UtxoSet => "utxo_set",
            Self::TxidLocation => "txid_location",
        }
    }
}

/// `kind`'s store on `network` (directory bound to that network: another one's = refused at open)
pub const fn schema(kind: IndexKind, format: u16, network: NetworkType, tables: Tables) -> Schema {
    let scope = match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    };
    Schema::new(kind.name(), format, scope, tables)
}
