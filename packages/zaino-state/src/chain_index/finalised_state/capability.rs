//! Capability model, versioned metadata, and DB trait surface
//!
//! This file defines the **capability- and version-aware interface** that all `ZainoDB` database
//! implementations must conform to.
//!
//! The core idea is:
//! - Each concrete DB major version (e.g. `DbV0`, `DbV1`) implements a common set of traits.
//! - A `Capability` bitmap declares which parts of that trait surface are actually supported.
//! - The router (`Router`) and reader (`DbReader`) use *single-feature* requests
//!   (`CapabilityRequest`) to route a call to a backend that is guaranteed to support it.
//!
//! This design enables:
//! - running mixed-version configurations during major migrations (primary + shadow),
//! - serving old data while building new indices,
//! - and gating API features cleanly when a backend does not support an extension.
//!
//! # What’s in this file
//!
//! ## Capability / routing types
//! - [`Capability`]: bitflags describing what an *open* database instance can serve.
//! - [`CapabilityRequest`]: a single-feature request (non-composite) used for routing.
//!
//! ## Versioned metadata
//! - [`DbVersion`]: schema version triple (major/minor/patch) plus a mapping to supported capabilities.
//! - [`DbMetadata`]: persisted singleton stored under the fixed key `"metadata"` in the LMDB
//!   metadata database; includes:
//!   - `version: DbVersion`
//!   - `schema_hash: [u8; 32]` (BLAKE2b-256 of schema definition/contract)
//!   - `migration_status: MigrationStatus`
//! - [`MigrationStatus`]: persisted migration progress marker to support resuming after shutdown.
//!
//! All metadata types in this file implement `ZainoVersionedSerde` and therefore have explicit
//! on-disk encoding versions.
//!
//! ## Trait surface
//! This file defines:
//!
//! - **Core traits** implemented by every DB version:
//!   - [`DbRead`], [`DbWrite`], and [`DbCore`]
//!
//! - **Extension traits** implemented by *some* versions:
//!   - [`BlockCoreExt`], [`BlockTransparentExt`], [`BlockShieldedExt`]
//!   - [`CompactBlockExt`]
//!   - [`IndexedBlockExt`]
//!   - [`TransparentHistExt`]
//!
//! Extension traits must be capability-gated: if a DB does not advertise the corresponding capability
//! bit, routing must not hand that backend out for that request.
//!
//! # Versioning strategy (practical guidance)
//!
//! - `DbVersion::major` is the primary compatibility boundary:
//!   - v0 is a legacy compact-block streamer.
//!   - v1 adds richer indices (chain block data + transparent history).
//!
//! - `minor`/`patch` can be used for additive or compatible changes, but only if on-disk encodings
//!   remain readable and all invariants remain satisfied.
//!
//! - `DbVersion::capability()` must remain conservative:
//!   - only advertise capabilities that are fully correct for that on-disk schema.
//!
//! # Development: adding or changing features safely
//!
//! When adding a new feature/query that requires new persistent data:
//!
//! 1. Add a new capability bit to [`Capability`].
//! 2. Add a corresponding variant to [`CapabilityRequest`] and map it in:
//!    - `as_capability()`
//!    - `name()`
//! 3. Add a new extension trait (or extend an existing one) that expresses the required operations.
//! 4. Implement the extension trait for the latest DB version(s).
//! 5. Update `DbVersion::capability()` for the version(s) that support it.
//! 6. Route it through `DbReader` by requesting the new `CapabilityRequest`.
//!
//! When changing persisted metadata formats, bump the `ZainoVersionedSerde::VERSION` for that type
//! and provide a decoding path in `decode_latest()`.

use core::fmt;

use crate::{
    chain_index::types::TransactionHash, error::FinalisedStateError, read_fixed_le, read_u32_le,
    read_u8, version, write_fixed_le, write_u32_le, write_u8, BlockHash, BlockHeaderData,
    CommitmentTreeData, CompactBlockStream, FixedEncodedLen, Height, IndexedBlock,
    OrchardCompactTx, OrchardTxList, SaplingCompactTx, SaplingTxList, StatusType,
    TransparentCompactTx, TransparentTxList, TxLocation, TxidList, ZainoVersionedSerde,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::types::AddrEventBytes, AddrScript, Outpoint};

use async_trait::async_trait;
use bitflags::bitflags;
use core2::io::{self, Read, Write};
use zaino_proto::proto::utils::PoolTypeFilter;

// ***** Capability definition structs *****

bitflags! {
    /// Capability bitmap describing what an **open** database instance can serve.
    ///
    /// A capability is an *implementation promise*: if a backend advertises a capability bit, then
    /// the corresponding trait surface must be fully and correctly implemented for that backend’s
    /// on-disk schema.
    ///
    /// ## How capabilities are used
    /// - [`DbVersion::capability`] maps a persisted schema version to a conservative capability set.
    /// - [`crate::chain_index::finalised_state::router::Router`] holds a primary and optional shadow
    ///   backend and uses masks to decide which backend may serve a given feature.
    /// - [`crate::chain_index::finalised_state::reader::DbReader`] requests capabilities via
    ///   [`CapabilityRequest`] (single-feature requests) and therefore obtains a backend that is
    ///   guaranteed to support the requested operation.
    ///
    /// ## Extension trait mapping
    /// Each bit corresponds 1-for-1 with a trait surface:
    /// - `READ_CORE` / `WRITE_CORE` correspond to [`DbRead`] / [`DbWrite`]
    /// - all other bits correspond to extension traits (e.g. [`BlockCoreExt`], [`TransparentHistExt`])
    #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
    pub(crate) struct Capability: u32 {
        /* ------ core database functionality ------ */

        /// Backend implements [`DbRead`].
        ///
        /// This includes:
        /// - tip height (`db_height`)
        /// - hash↔height lookups
        /// - reading the persisted metadata singleton.
        const READ_CORE             = 0b0000_0001;

        /// Backend implements [`DbWrite`].
        ///
        /// This includes:
        /// - appending tip blocks,
        /// - deleting tip blocks,
        /// - and updating the metadata singleton.
        const WRITE_CORE            = 0b0000_0010;

        /* ---------- database extensions ---------- */

        /// Backend implements [`BlockCoreExt`] (header/txid and tx-index lookups).
        const BLOCK_CORE_EXT        = 0b0000_0100;

        /// Backend implements [`BlockTransparentExt`] (transparent per-block/per-tx data).
        const BLOCK_TRANSPARENT_EXT = 0b0000_1000;

        /// Backend implements [`BlockShieldedExt`] (sapling/orchard per-block/per-tx data).
        const BLOCK_SHIELDED_EXT    = 0b0001_0000;

        /// Backend implements [`CompactBlockExt`] (CompactBlock materialization).
        const COMPACT_BLOCK_EXT     = 0b0010_0000;

        /// Backend implements [`IndexedBlockExt`] (full `IndexedBlock` materialization).
        const CHAIN_BLOCK_EXT       = 0b0100_0000;

        /// Backend implements [`TransparentHistExt`] (transparent address history indices).
        #[cfg(feature = "transparent_address_history_experimental")]
        const TRANSPARENT_HIST_EXT  = 0b1000_0000;
    }
}

impl Capability {
    /// Capability set supported by a **fresh** database at the latest major schema supported by this build.
    ///
    /// This value is used as the “expected modern baseline” for new DB instances. It must remain in
    /// sync with:
    /// - the latest on-disk schema (`DbV1` today, `DbV2` in the future),
    /// - and [`DbVersion::capability`] for that schema.
    pub(crate) const LATEST: Capability = {
        let base = Capability::READ_CORE
            .union(Capability::WRITE_CORE)
            .union(Capability::BLOCK_CORE_EXT)
            .union(Capability::BLOCK_TRANSPARENT_EXT)
            .union(Capability::BLOCK_SHIELDED_EXT)
            .union(Capability::COMPACT_BLOCK_EXT)
            .union(Capability::CHAIN_BLOCK_EXT);

        #[cfg(feature = "transparent_address_history_experimental")]
        {
            base.union(Capability::TRANSPARENT_HIST_EXT)
        }
        #[cfg(not(feature = "transparent_address_history_experimental"))]
        {
            base
        }
    };

    /// Returns `true` if `self` includes **all** bits from `other`.
    ///
    /// This is primarily used for feature gating and routing assertions.
    #[inline]
    pub(crate) const fn has(self, other: Capability) -> bool {
        self.contains(other)
    }
}

/// A *single-feature* capability request used for routing.
///
/// `CapabilityRequest` values are intentionally non-composite: each variant maps to exactly one
/// [`Capability`] bit. This keeps routing and error reporting unambiguous.
///
/// The router uses the request to select a backend that advertises the requested capability.
/// If no backend advertises the capability, the call must fail with
/// [`FinalisedStateError::FeatureUnavailable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CapabilityRequest {
    /// Request the [`DbRead`] core surface.
    ReadCore,

    /// Request the [`DbWrite`] core surface.
    WriteCore,

    /// Request the [`BlockCoreExt`] extension surface.
    BlockCoreExt,

    /// Request the [`BlockTransparentExt`] extension surface.
    BlockTransparentExt,

    /// Request the [`BlockShieldedExt`] extension surface.
    BlockShieldedExt,

    /// Request the [`CompactBlockExt`] extension surface.
    CompactBlockExt,

    /// Request the [`IndexedBlockExt`] extension surface.
    IndexedBlockExt,

    /// Request the [`TransparentHistExt`] extension surface.
    #[cfg(feature = "transparent_address_history_experimental")]
    TransparentHistExt,
}

impl CapabilityRequest {
    /// Maps this request to the corresponding single-bit [`Capability`].
    ///
    /// This mapping must remain 1-for-1 with:
    /// - the definitions in [`Capability`], and
    /// - the human-readable names returned by [`CapabilityRequest::name`].
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
            #[cfg(feature = "transparent_address_history_experimental")]
            CapabilityRequest::TransparentHistExt => Capability::TRANSPARENT_HIST_EXT,
        }
    }

    /// Returns a stable human-friendly feature name for errors and logs.
    ///
    /// This value is used in [`FinalisedStateError::FeatureUnavailable`] and must remain stable
    /// across refactors to avoid confusing diagnostics.
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
            #[cfg(feature = "transparent_address_history_experimental")]
            CapabilityRequest::TransparentHistExt => "TRANSPARENT_HIST_EXT",
        }
    }
}

/// Convenience conversion from a routing request to its single-bit capability.
impl From<CapabilityRequest> for Capability {
    #[inline]
    fn from(req: CapabilityRequest) -> Self {
        req.as_capability()
    }
}

// ***** Database metadata structs *****

/// Persisted database metadata singleton.
///
/// This record is stored under the fixed key `"metadata"` in the LMDB metadata database and is used to:
/// - identify the schema version currently on disk,
/// - bind the database to an explicit schema contract (`schema_hash`),
/// - and persist migration progress (`migration_status`) for crash-safe resumption.
///
/// ## Encoding
/// `DbMetadata` implements [`ZainoVersionedSerde`]. The encoded body is:
/// - one versioned [`DbVersion`],
/// - a fixed 32-byte schema hash,
/// - one versioned [`MigrationStatus`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct DbMetadata {
    /// Schema version triple for the on-disk database.
    pub(crate) version: DbVersion,

    /// BLAKE2b-256 hash of the schema definition/contract.
    ///
    /// This hash is intended to detect accidental schema drift (layout/type changes) across builds.
    /// It is not a security boundary; it is a correctness and operator-safety signal.
    pub(crate) schema_hash: [u8; 32],

    /// Persisted migration state, used to resume safely after shutdown/crash.
    ///
    /// Outside of migrations this should be [`MigrationStatus::Empty`].
    pub(crate) migration_status: MigrationStatus,
}

impl DbMetadata {
    /// Constructs a new metadata record.
    ///
    /// Callers should ensure `schema_hash` matches the schema contract for `version`, and that
    /// `migration_status` is set conservatively (typically `Empty` unless actively migrating).
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

    /// Returns the persisted schema version.
    pub(crate) fn version(&self) -> DbVersion {
        self.version
    }

    /// Returns the schema contract hash.
    pub(crate) fn schema(&self) -> [u8; 32] {
        self.schema_hash
    }

    /// Returns the persisted migration status.
    pub(crate) fn migration_status(&self) -> MigrationStatus {
        self.migration_status
    }
}

/// Versioned on-disk encoding for the metadata singleton.
///
/// Body layout (after the `ZainoVersionedSerde` tag byte):
/// 1. `DbVersion` (versioned, includes its own tag)
/// 2. `[u8; 32]` schema hash
/// 3. `MigrationStatus` (versioned, includes its own tag)
impl ZainoVersionedSerde for DbMetadata {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.version.serialize_with_version(&mut *w, 1)?;
        write_fixed_le::<32, _>(&mut *w, &self.schema_hash)?;
        self.migration_status.serialize_with_version(&mut *w, 1)
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

/// `DbMetadata` has a fixed encoded body length.
///
/// Body length = `DbVersion::VERSIONED_LEN` (12 + 1) + 32-byte schema hash
/// + `MigrationStatus::VERSIONED_LEN` (1 + 1) = 47 bytes.
impl FixedEncodedLen for DbMetadata {
    const ENCODED_LEN: usize = DbVersion::VERSIONED_LEN + 32 + MigrationStatus::VERSIONED_LEN;
}

/// Human-readable summary for logs.
///
/// The schema hash is abbreviated to the first 4 bytes for readability.
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

/// Database schema version triple.
///
/// The version is interpreted as `{major}.{minor}.{patch}` and is used to:
/// - select a database backend implementation,
/// - determine supported capabilities for routing,
/// - and enforce safe upgrades via migrations.
///
/// ## Compatibility model
/// - `major` is the primary compatibility boundary (schema family).
/// - `minor` and `patch` may be used for compatible changes, but only if all persisted record
///   encodings remain readable and correctness invariants are preserved.
///
/// The authoritative capability mapping is provided by [`DbVersion::capability`], and must remain
/// conservative: only advertise features that are correct for the given on-disk schema.
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
    /// Construct a new DbVersion.
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

    /// Returns the conservative capability set for this schema version.
    ///
    /// Routing relies on this mapping for safety: if a capability is not included here, callers
    /// must not assume the corresponding trait surface is available.
    ///
    /// If a schema version is unknown to this build, this returns [`Capability::empty`], ensuring
    /// the router will reject feature requests rather than serving incorrect data.
    pub(crate) fn capability(&self) -> Capability {
        match (self.major, self.minor) {
            // V0: legacy compact block streamer.
            (0, _) => {
                Capability::READ_CORE | Capability::WRITE_CORE | Capability::COMPACT_BLOCK_EXT
            }

            // V1: Adds chainblockv1 and transparent transaction history data.
            (1, 0) => {
                let base = Capability::READ_CORE
                    | Capability::WRITE_CORE
                    | Capability::BLOCK_CORE_EXT
                    | Capability::BLOCK_TRANSPARENT_EXT
                    | Capability::BLOCK_SHIELDED_EXT
                    | Capability::COMPACT_BLOCK_EXT
                    | Capability::CHAIN_BLOCK_EXT;

                #[cfg(feature = "transparent_address_history_experimental")]
                {
                    base | Capability::TRANSPARENT_HIST_EXT
                }
                #[cfg(not(feature = "transparent_address_history_experimental"))]
                {
                    base
                }
            }

            // Unknown / unsupported
            _ => Capability::empty(),
        }
    }
}

/// Versioned on-disk encoding for database versions.
///
/// Body layout (after the tag byte): three little-endian `u32` values:
/// `major`, `minor`, `patch`.
impl ZainoVersionedSerde for DbVersion {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u32_le(&mut *w, self.major)?;
        write_u32_le(&mut *w, self.minor)?;
        write_u32_le(&mut *w, self.patch)
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

// DbVersion: body = 3*(4-byte u32) - 12 bytes
impl FixedEncodedLen for DbVersion {
    const ENCODED_LEN: usize = 4 + 4 + 4;
}

/// Formats as `{major}.{minor}.{patch}` for logs and diagnostics.
impl core::fmt::Display for DbVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Persisted migration progress marker.
///
/// This value exists to make migrations crash-resumable. A migration may:
/// - build a shadow database incrementally,
/// - optionally perform partial rebuild phases to limit disk amplification,
/// - and finally promote the shadow to primary.
///
/// Database implementations and the migration manager must treat this value conservatively:
/// if the process is interrupted, the next startup should be able to determine the correct
/// resumption behavior from this status and the on-disk state.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
#[derive(Default)]
pub(crate) enum MigrationStatus {
    /// No migration is in progress.
    #[default]
    Empty,

    /// A partial build phase is currently in progress.
    ///
    /// Some migrations split work into phases to limit disk usage (for example, deleting the old
    /// database before rebuilding the new one in full).
    PartialBuidInProgress,

    /// The partial build phase completed successfully.
    PartialBuildComplete,

    /// The final build phase is currently in progress.
    FinalBuildInProgress,

    /// Migration work is complete and the database is ready for promotion/steady-state operation.
    Complete,
}

/// Human-readable migration status for logs and diagnostics.
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

/// Versioned on-disk encoding for migration status.
///
/// Body layout (after the tag byte): one `u8` discriminator.
/// Unknown tags must fail decoding.
impl ZainoVersionedSerde for MigrationStatus {
    const VERSION: u8 = version::V1;

    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
        Self::encode_v1(self, w)
    }

    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::decode_v1(r)
    }

    fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let tag = match self {
            MigrationStatus::Empty => 0,
            MigrationStatus::PartialBuidInProgress => 1,
            MigrationStatus::PartialBuildComplete => 2,
            MigrationStatus::FinalBuildInProgress => 3,
            MigrationStatus::Complete => 4,
        };
        write_u8(w, tag)
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

/// `MigrationStatus` has a fixed 1-byte encoded body (discriminator).
impl FixedEncodedLen for MigrationStatus {
    const ENCODED_LEN: usize = 1;
}

// ***** Core Database functionality *****

/// Core read-only operations that *every* database schema version must support.
///
/// These operations form the minimum required surface for:
/// - determining the chain tip stored on disk,
/// - mapping hashes to heights and vice versa,
/// - and reading the persisted schema metadata.
///
/// All methods must be consistent with the database’s *finalised* chain view.
#[async_trait]
pub trait DbRead: Send + Sync {
    /// Returns the highest block height stored, or `None` if the database is empty.
    ///
    /// Implementations must treat the stored height as the authoritative tip for all other core
    /// lookups.
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError>;

    /// Returns the height for `hash` if present.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError>;

    /// Returns the hash for `height` if present.
    ///
    /// Returns:
    /// - `Ok(Some(hash))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError>;

    /// Returns the persisted metadata singleton.
    ///
    /// This must reflect the schema actually used by the backend instance.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError>;
}

/// Core write operations that *every* database schema version must support.
///
/// The finalised database is updated using *stack semantics*:
/// - blocks are appended at the tip (`write_block`),
/// - and removed only from the tip (`delete_block_at_height` / `delete_block`).
///
/// Implementations must keep all secondary indices internally consistent with these operations.
#[async_trait]
pub trait DbWrite: Send + Sync {
    /// Appends a fully-validated block to the database.
    ///
    /// Invariant: `block` must be the next height after the current tip (no gaps, no rewrites).
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError>;

    /// Deletes the tip block identified by `height` from every finalised table.
    ///
    /// Invariant: `height` must be the current database tip height.
    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError>;

    /// Deletes the provided tip block from every finalised table.
    ///
    /// This is the “full-information” deletion path: it takes an [`IndexedBlock`] so the backend
    /// can deterministically remove all derived index entries even if reconstructing them from
    /// height alone is not possible.
    ///
    /// Invariant: `block` must be the current database tip block.
    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError>;

    /// Replaces the persisted metadata singleton with `metadata`.
    ///
    /// Implementations must ensure this update is atomic with respect to readers (within the
    /// backend’s concurrency model).
    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError>;
}

/// Core runtime surface implemented by every backend instance.
///
/// This trait binds together:
/// - the core read/write operations, and
/// - lifecycle and status reporting for background tasks.
///
/// In practice, [`crate::chain_index::finalised_state::router::Router`] implements this by
/// delegating to the currently routed core backend(s).
#[async_trait]
pub trait DbCore: DbRead + DbWrite + Send + Sync {
    /// Returns the current runtime status (`Starting`, `Syncing`, `Ready`, …).
    fn status(&self) -> StatusType;

    /// Initiates a graceful shutdown of background tasks and closes database resources.
    async fn shutdown(&self) -> Result<(), FinalisedStateError>;
}

// ***** Database Extension traits *****

/// Core block indexing extension.
///
/// This extension covers header and txid range fetches plus transaction indexing by [`TxLocation`].
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise [`Capability::BLOCK_CORE_EXT`].
#[async_trait]
pub trait BlockCoreExt: Send + Sync {
    /// Return block header data by height.
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError>;

    /// Returns block headers for the inclusive range `[start, end]`.
    ///
    /// Callers should ensure `start <= end`.
    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError>;

    /// Return block txids by height.
    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError>;

    /// Return block txids for the given height range.
    ///
    /// Callers should ensure `start <= end`.
    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError>;

    /// Returns the transaction hash for the given [`TxLocation`].
    ///
    /// `TxLocation` is the internal transaction index key used by the database.
    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError>;

    /// Returns the [`TxLocation`] for `txid` if the transaction is indexed.
    ///
    /// Returns:
    /// - `Ok(Some(location))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    ///
    /// NOTE: transaction data is indexed by TxLocation internally.
    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError>;
}

/// Transparent transaction indexing extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_TRANSPARENT_EXT`].
#[async_trait]
pub trait BlockTransparentExt: Send + Sync {
    /// Returns the serialized [`TransparentCompactTx`] for `tx_location`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(tx))` if present,
    /// - `Ok(None)` if not present (not an error).
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError>;

    /// Fetch block transparent transaction data for given block height.
    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError>;

    /// Returns transparent transaction tx data for the inclusive block height range `[start, end]`.
    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError>;
}

/// Shielded transaction indexing extension (Sapling + Orchard + commitment tree data).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_SHIELDED_EXT`].
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

    /// Fetches block sapling tx data for the given (inclusive) height range.
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

    /// Fetches block orchard tx data for the given (inclusive) height range.
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

    /// Fetches block commitment tree data for the given (inclusive) height range.
    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError>;
}

/// CompactBlock materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::COMPACT_BLOCK_EXT`].
#[async_trait]
pub trait CompactBlockExt: Send + Sync {
    /// Returns the CompactBlock for the given Height.
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError>;

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError>;
}

/// `IndexedBlock` materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::CHAIN_BLOCK_EXT`].
#[async_trait]
pub trait IndexedBlockExt: Send + Sync {
    /// Returns the [`IndexedBlock`] for `height`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(block))` if present,
    /// - `Ok(None)` if not present (not an error).
    ///
    /// TODO: Add separate range fetch method as this method is slow for fetching large ranges!
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError>;
}

/// Transparent address history indexing extension.
///
/// This extension provides address-scoped queries backed by persisted indices built from the
/// transparent transaction graph (outputs, spends, and derived address events).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::TRANSPARENT_HIST_EXT`].
///
/// Range semantics:
/// - Methods that accept `start_height` and `end_height` interpret the range as inclusive:
///   `[start_height, end_height]`
#[cfg(feature = "transparent_address_history_experimental")]
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
