//! Holds ZainoDB capability traits and bitmaps.

use core::fmt;

use crate::{
    chain_index::types::{AddrEventBytes, TransactionHash},
    error::FinalisedStateError,
    read_fixed_le, read_u32_le, read_u8, version, write_fixed_le, write_u32_le, write_u8,
    AddrScript, BlockHash, BlockHeaderData, CommitmentTreeData, FixedEncodedLen, Height,
    IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList,
    StatusType, TransparentCompactTx, TransparentTxList, TxLocation, TxidList, ZainoVersionedSerde,
};

use async_trait::async_trait;
use bitflags::bitflags;
use core2::io::{self, Read, Write};

// ***** Capability definition structs *****

bitflags! {
    /// Represents what an **open** ZainoDB can provide.
    ///
    /// The façade (`ZainoDB`) sets these flags **once** at open-time from the
    /// on-disk `SchemaVersion`, then consults them to decide which helper
    /// (`writer()`, `block_core()`, …) it may expose.
    ///
    /// Each flag corresponds 1-for-1 with an extension trait.
    #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
    pub(crate) struct Capability: u32 {
        /* ------ core database functionality ------ */
        /// Implements `DbRead`.
        const READ_CORE             = 0b0000_0001;
        /// Implements `DbWrite`.
        const WRITE_CORE            = 0b0000_0010;

        /* ---------- database extensions ---------- */
        /// Implements `BlockCoreExt`.
        const BLOCK_CORE_EXT        = 0b0000_0100;
        /// Implements `BlockTransparentExt`.
        const BLOCK_TRANSPARENT_EXT = 0b0000_1000;
        /// Implements `BlockShieldedExt`.
        const BLOCK_SHIELDED_EXT    = 0b0001_0000;
        /// Implements `CompactBlockExt`.
        const COMPACT_BLOCK_EXT     = 0b0010_0000;
        /// Implements `IndexedBlockExt`.
        const CHAIN_BLOCK_EXT       = 0b0100_0000;
        /// Implements `TransparentHistExt`.
        const TRANSPARENT_HIST_EXT  = 0b1000_0000;
    }
}

impl Capability {
    /// All features supported by a **fresh v1** database.
    pub(crate) const LATEST: Capability = Capability::READ_CORE
        .union(Capability::WRITE_CORE)
        .union(Capability::BLOCK_CORE_EXT)
        .union(Capability::BLOCK_TRANSPARENT_EXT)
        .union(Capability::BLOCK_SHIELDED_EXT)
        .union(Capability::COMPACT_BLOCK_EXT)
        .union(Capability::CHAIN_BLOCK_EXT)
        .union(Capability::TRANSPARENT_HIST_EXT);

    /// Checks for the given capability.
    #[inline]
    pub(crate) const fn has(self, other: Capability) -> bool {
        self.contains(other)
    }
}

// A single-feature request type (cannot be composite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CapabilityRequest {
    ReadCore,
    WriteCore,
    BlockCoreExt,
    BlockTransparentExt,
    BlockShieldedExt,
    CompactBlockExt,
    IndexedBlockExt,
    TransparentHistExt,
}

impl CapabilityRequest {
    /// Map to the corresponding single-bit `Capability`.
    #[inline]
    pub(crate) const fn as_capability(self) -> Capability {
        match self {
            CapabilityRequest::ReadCore => Capability::READ_CORE,
            CapabilityRequest::WriteCore => Capability::WRITE_CORE,
            CapabilityRequest::BlockCoreExt => Capability::BLOCK_CORE_EXT,
            CapabilityRequest::BlockTransparentExt => Capability::BLOCK_TRANSPARENT_EXT,
            CapabilityRequest::BlockShieldedExt => Capability::BLOCK_SHIELDED_EXT,
            CapabilityRequest::CompactBlockExt => Capability::COMPACT_BLOCK_EXT,
            CapabilityRequest::IndexedBlockExt => Capability::CHAIN_BLOCK_EXT,
            CapabilityRequest::TransparentHistExt => Capability::TRANSPARENT_HIST_EXT,
        }
    }

    /// Human-friendly feature name for errors and logs.
    #[inline]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            CapabilityRequest::ReadCore => "READ_CORE",
            CapabilityRequest::WriteCore => "WRITE_CORE",
            CapabilityRequest::BlockCoreExt => "BLOCK_CORE_EXT",
            CapabilityRequest::BlockTransparentExt => "BLOCK_TRANSPARENT_EXT",
            CapabilityRequest::BlockShieldedExt => "BLOCK_SHIELDED_EXT",
            CapabilityRequest::CompactBlockExt => "COMPACT_BLOCK_EXT",
            CapabilityRequest::IndexedBlockExt => "CHAIN_BLOCK_EXT",
            CapabilityRequest::TransparentHistExt => "TRANSPARENT_HIST_EXT",
        }
    }
}

// Optional convenience conversions.
impl From<CapabilityRequest> for Capability {
    #[inline]
    fn from(req: CapabilityRequest) -> Self {
        req.as_capability()
    }
}

/// Top-level database metadata entry, storing the current schema version.
///
/// Stored under the fixed key `"metadata"` in the LMDB metadata database.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct DbMetadata {
    /// Encodes the version and schema hash.
    pub(crate) version: DbVersion,
    /// BLAKE2b-256 hash of the schema definition (includes struct layout, types, etc.)
    pub(crate) schema_hash: [u8; 32],
    /// Migration status of the database, `Empty` outside of migrations.
    pub(crate) migration_status: MigrationStatus,
}

impl DbMetadata {
    /// Creates a new DbMetadata.
    pub(crate) fn new(
        version: DbVersion,
        schema_hash: [u8; 32],
        migration_status: MigrationStatus,
    ) -> Self {
        Self {
            version,
            schema_hash,
            migration_status,
        }
    }

    /// Returns the version data.
    pub(crate) fn version(&self) -> DbVersion {
        self.version
    }

    /// Returns the version schema hash.
    pub(crate) fn schema(&self) -> [u8; 32] {
        self.schema_hash
    }

    /// Returns the migration status of the database.
    pub(crate) fn migration_status(&self) -> MigrationStatus {
        self.migration_status
    }
}

impl ZainoVersionedSerde for DbMetadata {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.version.serialize(&mut *w)?;
        write_fixed_le::<32, _>(&mut *w, &self.schema_hash)?;
        self.migration_status.serialize(&mut *w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let version = DbVersion::deserialize(&mut *r)?;
        let schema_hash = read_fixed_le::<32, _>(&mut *r)?;
        let migration_status = MigrationStatus::deserialize(&mut *r)?;
        Ok(DbMetadata {
            version,
            schema_hash,
            migration_status,
        })
    }
}

// DbMetadata: its body is one *versioned* DbVersion (12 + 1 tag) + 32-byte schema hash
// + one *versioned* MigrationStatus (1 + 1 tag) = 47 bytes
impl FixedEncodedLen for DbMetadata {
    const ENCODED_LEN: usize = DbVersion::VERSIONED_LEN + 32 + MigrationStatus::VERSIONED_LEN;
}

impl core::fmt::Display for DbMetadata {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "DbMetadata {{ version: {}.{}.{} , schema_hash: 0x",
            self.version.major(),
            self.version.minor(),
            self.version.patch()
        )?;

        for byte in &self.schema_hash[..4] {
            write!(f, "{byte:02x}")?;
        }

        write!(f, "… }}")
    }
}

/// Database schema version information.
///
/// This is used for schema migration safety and compatibility checks.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct DbVersion {
    /// Major version tag.
    pub(crate) major: u32,
    /// Minor version tag.
    pub(crate) minor: u32,
    /// Patch tag.
    pub(crate) patch: u32,
}

impl DbVersion {
    /// creates a new DbVersion.
    pub(crate) fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Returns the major version tag.
    pub(crate) fn major(&self) -> u32 {
        self.major
    }

    /// Returns the minor version tag.
    pub(crate) fn minor(&self) -> u32 {
        self.minor
    }

    /// Returns the patch tag.
    pub(crate) fn patch(&self) -> u32 {
        self.patch
    }

    pub(crate) fn capability(&self) -> Capability {
        match (self.major, self.minor) {
            // V0: legacy compact block streamer.
            (0, _) => {
                Capability::READ_CORE | Capability::WRITE_CORE | Capability::COMPACT_BLOCK_EXT
            }

            // V1: Adds chainblockv1 and transparent transaction history data.
            (1, 0) => {
                Capability::READ_CORE
                    | Capability::WRITE_CORE
                    | Capability::BLOCK_CORE_EXT
                    | Capability::BLOCK_TRANSPARENT_EXT
                    | Capability::BLOCK_SHIELDED_EXT
                    | Capability::COMPACT_BLOCK_EXT
                    | Capability::CHAIN_BLOCK_EXT
                    | Capability::TRANSPARENT_HIST_EXT
            }

            // Unknown / unsupported
            _ => Capability::empty(),
        }
    }
}

impl ZainoVersionedSerde for DbVersion {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u32_le(&mut *w, self.major)?;
        write_u32_le(&mut *w, self.minor)?;
        write_u32_le(&mut *w, self.patch)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        let major = read_u32_le(&mut *r)?;
        let minor = read_u32_le(&mut *r)?;
        let patch = read_u32_le(&mut *r)?;
        Ok(DbVersion {
            major,
            minor,
            patch,
        })
    }
}

/* DbVersion: body = 3*(4-byte u32) - 12 bytes */
impl FixedEncodedLen for DbVersion {
    const ENCODED_LEN: usize = 4 + 4 + 4;
}

impl core::fmt::Display for DbVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Holds migration data.
///
/// This is used when the database is shutdown mid-migration to ensure migration correctness.
///
/// NOTE: Some migrations run a partial database rebuild before the final build process.
///       This is done to minimise disk requirements during migrations,
///       enabling the deletion of the old database before the the database is rebuilt in full.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
#[derive(Default)]
pub(crate) enum MigrationStatus {
    #[default]
    Empty,
    PartialBuidInProgress,
    PartialBuildComplete,
    FinalBuildInProgress,
    Complete,
}

impl fmt::Display for MigrationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status_str = match self {
            MigrationStatus::Empty => "Empty",
            MigrationStatus::PartialBuidInProgress => "Partial build in progress",
            MigrationStatus::PartialBuildComplete => "Partial build complete",
            MigrationStatus::FinalBuildInProgress => "Final build in progress",
            MigrationStatus::Complete => "Complete",
        };
        write!(f, "{status_str}")
    }
}

impl ZainoVersionedSerde for MigrationStatus {
    const VERSION: u8 = version::V1;

    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let tag = match self {
            MigrationStatus::Empty => 0,
            MigrationStatus::PartialBuidInProgress => 1,
            MigrationStatus::PartialBuildComplete => 2,
            MigrationStatus::FinalBuildInProgress => 3,
            MigrationStatus::Complete => 4,
        };
        write_u8(w, tag)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        match read_u8(r)? {
            0 => Ok(MigrationStatus::Empty),
            1 => Ok(MigrationStatus::PartialBuidInProgress),
            2 => Ok(MigrationStatus::PartialBuildComplete),
            3 => Ok(MigrationStatus::FinalBuildInProgress),
            4 => Ok(MigrationStatus::Complete),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid MigrationStatus tag: {other}"),
            )),
        }
    }
}

impl FixedEncodedLen for MigrationStatus {
    const ENCODED_LEN: usize = 1;
}

// ***** Core Database functionality *****

/// Read-only operations that *every* ZainoDB version must support.
#[async_trait]
pub trait DbRead: Send + Sync {
    /// Highest block height stored (or `None` if DB empty).
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError>;

    /// Lookup height of a block by its hash.
    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError>;

    /// Lookup hash of a block by its height.
    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError>;

    /// Return the persisted `DbMetadata` singleton.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError>;
}

/// Write operations that *every* ZainoDB version must support.
#[async_trait]
pub trait DbWrite: Send + Sync {
    /// Persist a fully-validated block to the database.
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError>;

    /// Deletes a block identified height from every finalised table.
    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError>;

    /// Wipe the given block data from every finalised table.
    ///
    /// Takes a IndexedBlock as input and ensures all data from this block is wiped from the database.
    ///
    /// Used as a backup when delete_block_at_height fails.
    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError>;

    /// Update the metadata store with the given DbMetadata
    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError>;
}

/// Core database functionality that *every* ZainoDB version must support.
#[async_trait]
pub trait DbCore: DbRead + DbWrite + Send + Sync {
    /// Returns the current runtime status (`Starting`, `Syncing`, `Ready`, …).
    fn status(&self) -> StatusType;

    /// Stops background tasks, syncs, etc.
    async fn shutdown(&self) -> Result<(), FinalisedStateError>;
}

// ***** Database Extension traits *****

/// Core block data extension.
#[async_trait]
pub trait BlockCoreExt: Send + Sync {
    /// Return block header data by height.
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError>;

    /// Return block headers for the given height range.
    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError>;

    /// Return block txids by height.
    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError>;

    /// Return block txids for the given height range.
    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError>;

    /// Fetch the txid bytes for a given TxLocation.
    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError>;

    /// Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError>;
}

/// Transparent block data extension.
#[async_trait]
pub trait BlockTransparentExt: Send + Sync {
    /// Fetch the serialized TransparentCompactTx for the given TxLocation, if present.
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError>;

    /// Fetch block transparent transaction data by height.
    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError>;

    /// Fetches block transparent tx data for the given height range.
    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError>;
}

/// Transparent block data extension.
#[async_trait]
pub trait BlockShieldedExt: Send + Sync {
    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError>;

    /// Fetch block sapling transaction data by height.
    async fn get_block_sapling(&self, height: Height)
        -> Result<SaplingTxList, FinalisedStateError>;

    /// Fetches block sapling tx data for the given height range.
    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError>;

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError>;

    /// Fetch block orchard transaction data by height.
    async fn get_block_orchard(&self, height: Height)
        -> Result<OrchardTxList, FinalisedStateError>;

    /// Fetches block orchard tx data for the given height range.
    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError>;

    /// Fetch block commitment tree data by height.
    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError>;

    /// Fetches block commitment tree data for the given height range.
    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError>;
}

/// CompactBlock extension.
#[async_trait]
pub trait CompactBlockExt: Send + Sync {
    /// Returns the CompactBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError>;
}

/// IndexedBlock v1 extension.
#[async_trait]
pub trait IndexedBlockExt: Send + Sync {
    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError>;
}

/// IndexedBlock v1 extension.
#[async_trait]
pub trait TransparentHistExt: Send + Sync {
    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError>;

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError>;

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError>;

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError>;

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, FinalisedStateError>;

    // TODO: Add addr_deltas_by_range method!

    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError>;

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError>;
}
