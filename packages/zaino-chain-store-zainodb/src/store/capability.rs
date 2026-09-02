//! Capability model, versioned metadata, and DB trait surface
//!
//! This file defines the **capability- and version-aware interface** that all `FinalisedState` database
//! implementations must conform to.
//!
//! The core idea is:
//! - Each concrete DB major version (e.g. `DbV1`) implements a common set of traits.
//! - A `Capability` bitmap declares which parts of that trait surface are actually supported.
//! - The router (`Router`) and reader (`DbReader`) use *single-feature* requests
//!   (`CapabilityRequest`) to route a call to a backend that is guaranteed to support it.
//!
//! This design enables:
//! - reporting reduced capability while a migration is under way,
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
//!   - v1 is the current schema (chain block data + transparent history).
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

use crate::error::StoreError;
use crate::stream::CompactBlockStream;
use crate::support::SendFut;
use crate::types::{
    db::metadata::FinalisedTxOutSetInfoAccumulator, BlockHash, BlockHeaderData, CommitmentTreeData,
    Height, IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx,
    SaplingTxList, TransactionHash, TransparentCompactTx, TransparentTxList, TxLocation,
    TxOutCompact, TxidList,
};
use zaino_encoding::{
    read_fixed_le, read_u32_le, read_u8, version, write_fixed_le, write_u32_le, write_u8,
    FixedEncodedLen, ZainoVersionedSerde,
};
use zaino_status::StatusType;

#[cfg(feature = "transparent_address_history_experimental")]
use crate::types::{AddrEventBytes, AddrScript};

use bitflags::bitflags;
use corez::io::{self, Read, Write};
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
    /// - [`crate::store::router::Router`] holds a primary and optional ephemeral
    ///   backend and uses masks to decide which backend may serve a given feature.
    /// - [`crate::store::reader::DbReader`] requests capabilities via
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

        /// Backend advertises no supported capability bits.
        const NONE                  = 0;

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
        ///
        /// Address history only. It used to also stand for the spent-output
        /// index and the txout-set accumulator, which are neither address
        /// history nor experimental — see [`Capability::SPENT_OUTPUT_INDEX`].
        const TRANSPARENT_HIST_INDEX = 0b1000_0000;

        /// Backend implements [`SpentOutputExt`] (which transaction spent an outpoint).
        ///
        /// Split out of `TRANSPARENT_HIST_EXT`, which conflated three things.
        /// The spent index is built unconditionally from schema v1.2 onward and
        /// has nothing to do with address history; routing it through a bit
        /// named after an experimental feature meant a build without that
        /// feature advertised a capability under a name that implied otherwise.
        const SPENT_OUTPUT_INDEX    = 0b0001_0000_0000;

        /// Backend implements [`TxOutSetExt`] (the UTXO-set accumulator).
        ///
        /// Separate from [`Capability::SPENT_OUTPUT_INDEX`] because it is a
        /// separate persisted row that a backend could maintain without the
        /// other, and because its correctness condition is different: the
        /// accumulator is a running fold, so a backend that has one is claiming
        /// it has been maintained across every write, not merely that a table
        /// exists.
        const TXOUT_SET_INDEX       = 0b0010_0000_0000;
    }
}

impl Capability {
    /// Every capability a fresh database at the latest schema serves, except
    /// address history.
    ///
    /// Split from [`Capability::LATEST`] so the address-history bit is added in
    /// exactly one place rather than being repeated in two `cfg` arms.
    const LATEST_WITHOUT_ADDRESS_HISTORY: Capability = Capability::READ_CORE
        .union(Capability::WRITE_CORE)
        .union(Capability::BLOCK_CORE_EXT)
        .union(Capability::BLOCK_TRANSPARENT_EXT)
        .union(Capability::BLOCK_SHIELDED_EXT)
        .union(Capability::COMPACT_BLOCK_EXT)
        .union(Capability::CHAIN_BLOCK_EXT)
        .union(Capability::SPENT_OUTPUT_INDEX)
        .union(Capability::TXOUT_SET_INDEX);

    /// Capability set supported by a **fresh** database at the latest major schema
    /// supported by this build.
    ///
    /// The expected modern baseline for new database instances. It must remain in sync
    /// with the latest on-disk schema (`DbV1` today, `DbV2` in the future) and with
    /// [`DbVersion::capability`] for that schema; a test asserts the two agree.
    ///
    /// This arm: address history is compiled in, so a fresh database serves it.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) const LATEST: Capability =
        Capability::LATEST_WITHOUT_ADDRESS_HISTORY.union(Capability::TRANSPARENT_HIST_INDEX);

    /// As above, but address history is not compiled in, so no database can
    /// serve it — the reads do not exist in this build.
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    pub(crate) const LATEST: Capability = Capability::LATEST_WITHOUT_ADDRESS_HISTORY;

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
/// [`StoreError::FeatureUnavailable`].
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
    TransparentHistIndex,

    /// Request the [`SpentOutputExt`] extension surface.
    SpentOutputIndex,

    /// Request the [`TxOutSetExt`] extension surface.
    TxOutSetIndex,
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
            CapabilityRequest::TransparentHistIndex => Capability::TRANSPARENT_HIST_INDEX,
            CapabilityRequest::SpentOutputIndex => Capability::SPENT_OUTPUT_INDEX,
            CapabilityRequest::TxOutSetIndex => Capability::TXOUT_SET_INDEX,
        }
    }

    /// Returns a stable human-friendly feature name for errors and logs.
    ///
    /// This value is used in [`StoreError::FeatureUnavailable`] and must remain stable
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
            CapabilityRequest::TransparentHistIndex => "TRANSPARENT_HIST_INDEX",
            CapabilityRequest::SpentOutputIndex => "SPENT_OUTPUT_INDEX",
            CapabilityRequest::TxOutSetIndex => "TXOUT_SET_INDEX",
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
// `pub` (not `pub(crate)`) so it matches the visibility of the `pub` capability
// traits in this module that expose it (e.g. `DbRead::get_metadata`). The
// `capability` module is itself `pub(crate)`, so this does not widen the type
// beyond the crate; it only resolves the rustc-1.96 E0446 private-in-public
// check on `DbRead::get_metadata`'s signature.
pub struct DbMetadata {
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

/// Fixed-length encoding metadata for `DbMetadata`.
///
/// v1 consists of:
/// Body length = `DbVersion::VERSIONED_LEN` (12 + 1) + 32-byte schema hash
/// + `MigrationStatus::VERSIONED_LEN` (1 + 1) = 47 bytes.
impl FixedEncodedLen for DbMetadata {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(47),
            _ => None,
        }
    }
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
        // Everything every known v1 schema serves. The versions differ only in
        // the transparent indexes below, so the shared part is named once.
        let block_surfaces = Capability::READ_CORE
            | Capability::WRITE_CORE
            | Capability::BLOCK_CORE_EXT
            | Capability::BLOCK_TRANSPARENT_EXT
            | Capability::BLOCK_SHIELDED_EXT
            | Capability::COMPACT_BLOCK_EXT
            | Capability::CHAIN_BLOCK_EXT;

        // Address history exists only when compiled in: the reads are behind
        // the feature, so a build without it cannot serve them from any schema.
        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = Capability::TRANSPARENT_HIST_INDEX;
        #[cfg(not(feature = "transparent_address_history_experimental"))]
        let address_history = Capability::empty();

        let transparent_indexes = Capability::SPENT_OUTPUT_INDEX | Capability::TXOUT_SET_INDEX;

        match (self.major, self.minor) {
            // v1.0 / v1.1: the spent index and the txout-set accumulator were
            // built only when the address-history feature was on, so without it
            // a database of this vintage genuinely does not have them.
            (1, 0) | (1, 1) if address_history.is_empty() => block_surfaces,
            (1, 0) | (1, 1) => block_surfaces | transparent_indexes | address_history,

            // v1.2 moved the spent index out of the address-history feature, so
            // it and the accumulator are present regardless of the build.
            //
            // v1.3 (Ironwood / NU6.3) adds an ironwood commitment root, size and
            // tx row. All three are read through `BlockShieldedExt`, which v1.2
            // already advertises, so the version gained no capability and shares
            // this arm rather than duplicating it.
            (1, 2) | (1, 3) => block_surfaces | transparent_indexes | address_history,

            // Unknown / unsupported. Fails closed: the router rejects every
            // feature request rather than serving from a schema this build
            // cannot reason about.
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

/// Fixed-length encoding metadata for `DbVersion`.
///
/// v1 consists of *(4-byte u32) = 12 bytes
impl FixedEncodedLen for DbVersion {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(12),
            _ => None,
        }
    }
}

/// Formats as `{major}.{minor}.{patch}` for logs and diagnostics.
impl core::fmt::Display for DbVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Persisted migration progress marker.
///
/// This value exists to make migrations crash-resumable, which they must be:
/// they run in place on the one database, so a process that dies part-way
/// through has no untouched copy to fall back to. A migration may:
/// - rebuild the affected tables in place,
/// - optionally split that into phases to limit disk amplification.
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
    PartialBuildInProgress,

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
            MigrationStatus::PartialBuildInProgress => "Partial build in progress",
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
            MigrationStatus::PartialBuildInProgress => 1,
            MigrationStatus::PartialBuildComplete => 2,
            MigrationStatus::FinalBuildInProgress => 3,
            MigrationStatus::Complete => 4,
        };
        write_u8(w, tag)
    }

    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        match read_u8(r)? {
            0 => Ok(MigrationStatus::Empty),
            1 => Ok(MigrationStatus::PartialBuildInProgress),
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

/// Fixed-length encoding metadata for `MigrationStatus`.
///
/// v1 consists of a single byte
impl FixedEncodedLen for MigrationStatus {
    fn encoded_len(version: u8) -> Option<usize> {
        match version {
            version::V1 => Some(1),
            _ => None,
        }
    }
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
pub trait DbRead: Send + Sync {
    /// Returns the highest block height stored, or `None` if the database is empty.
    ///
    /// Implementations must treat the stored height as the authoritative tip for all other core
    /// lookups.
    fn db_height(&self) -> impl SendFut<Result<Option<Height>, StoreError>>;

    /// Returns the height for `hash` if present.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    fn get_block_height(&self, hash: BlockHash)
        -> impl SendFut<Result<Option<Height>, StoreError>>;

    /// Returns the hash for `height` if present.
    ///
    /// Returns:
    /// - `Ok(Some(hash))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    fn get_block_hash(&self, height: Height)
        -> impl SendFut<Result<Option<BlockHash>, StoreError>>;

    /// Returns the persisted metadata singleton.
    ///
    /// This must reflect the schema actually used by the backend instance.
    fn get_metadata(&self) -> impl SendFut<Result<DbMetadata, StoreError>>;
}

/// Core write operations that *every* database schema version must support.
///
/// The finalised database is updated using *stack semantics*:
/// - blocks are appended at the tip (`write_block`),
/// - and removed only from the tip (`delete_block_at_height` / `delete_block`).
///
/// Implementations must keep all secondary indices internally consistent with these operations.
pub trait DbWrite: Send + Sync {
    /// Appends a fully-validated block to the database.
    ///
    /// Invariant: `block` must be the next height after the current tip (no gaps, no rewrites).
    fn write_block(&self, block: IndexedBlock) -> impl SendFut<Result<(), StoreError>>;

    /// Ingests blocks from `source`, writing every height from the current tip up to and including
    /// `height` in order.
    ///
    /// This is the bulk catch-up path. Implementations own the ingestion loop so they can choose an
    /// efficient strategy: the v1 backend defers expensive secondary-index maintenance (the
    /// txout-set accumulator) across the run and rebuilds it once at the tip, whereas legacy
    /// backends may simply loop [`DbWrite::write_block`]. A no-op is valid when the tip already
    /// meets or exceeds `height`.
    fn write_blocks_to_height<S: zaino_chain_store::ChainStoreSource>(
        &self,
        height: Height,
        source: &S,
    ) -> impl SendFut<Result<(), StoreError>>;

    /// Deletes the tip block identified by `height` from every finalised table.
    ///
    /// Invariant: `height` must be the current database tip height.
    fn delete_block_at_height(&self, height: Height) -> impl SendFut<Result<(), StoreError>>;

    /// Deletes the provided tip block from every finalised table.
    ///
    /// This is the “full-information” deletion path: it takes an [`IndexedBlock`] so the backend
    /// can deterministically remove all derived index entries even if reconstructing them from
    /// height alone is not possible.
    ///
    /// Invariant: `block` must be the current database tip block.
    fn delete_block(&self, block: &IndexedBlock) -> impl SendFut<Result<(), StoreError>>;

    /// Replaces the persisted metadata singleton with `metadata`.
    ///
    /// Implementations must ensure this update is atomic with respect to readers (within the
    /// backend’s concurrency model).
    fn update_metadata(&self, metadata: DbMetadata) -> impl SendFut<Result<(), StoreError>>;
}

/// Core runtime surface implemented by every backend instance.
///
/// This trait binds together:
/// - the core read/write operations, and
/// - lifecycle and status reporting for background tasks.
///
/// In practice, [`crate::store::router::Router`] implements this by
/// delegating to the currently routed core backend(s).
pub trait DbCore: DbRead + DbWrite + Send + Sync {
    /// Returns the current runtime status (`Starting`, `Syncing`, `Ready`, …).
    fn status(&self) -> StatusType;

    /// Initiates a graceful shutdown of background tasks and closes database resources.
    fn shutdown(&self) -> impl SendFut<Result<(), StoreError>>;
}

// ***** Database Extension traits *****

/// Core block indexing extension.
///
/// This extension covers header and txid range fetches plus transaction indexing by [`TxLocation`].
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise [`Capability::BLOCK_CORE_EXT`].
pub trait BlockCoreExt: Send + Sync {
    /// Return block header data by height.
    fn get_block_header(&self, height: Height)
        -> impl SendFut<Result<BlockHeaderData, StoreError>>;

    /// Returns block headers for the inclusive range `[start, end]`.
    ///
    /// Callers should ensure `start <= end`.
    fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<BlockHeaderData>, StoreError>>;

    /// Return block txids by height.
    fn get_block_txids(&self, height: Height) -> impl SendFut<Result<TxidList, StoreError>>;

    /// Return block txids for the given height range.
    ///
    /// Callers should ensure `start <= end`.
    fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<TxidList>, StoreError>>;

    /// Returns the transaction hash for the given [`TxLocation`].
    ///
    /// `TxLocation` is the internal transaction index key used by the database.
    fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<TransactionHash, StoreError>>;

    /// Returns the [`TxLocation`] for `txid` if the transaction is indexed.
    ///
    /// Returns:
    /// - `Ok(Some(location))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    ///
    /// NOTE: transaction data is indexed by TxLocation internally.
    fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> impl SendFut<Result<Option<TxLocation>, StoreError>>;
}

/// Transparent transaction indexing extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_TRANSPARENT_EXT`].
pub trait BlockTransparentExt: Send + Sync {
    /// Returns the serialized [`TransparentCompactTx`] for `tx_location`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(tx))` if present,
    /// - `Ok(None)` if not present (not an error).
    fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<TransparentCompactTx>, StoreError>>;

    /// Fetch block transparent transaction data for given block height.
    fn get_block_transparent(
        &self,
        height: Height,
    ) -> impl SendFut<Result<TransparentTxList, StoreError>>;

    /// Returns transparent transaction tx data for the inclusive block height range `[start, end]`.
    fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<TransparentTxList>, StoreError>>;

    /// Returns the [`TxOutCompact`] referenced by `outpoint`, looking up the previous
    /// transaction's transparent data via the txid index and the transparent block table.
    ///
    /// Returns an error if the previous transaction is not indexed by the finalised state
    /// or the requested output index is out of range.
    fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> impl SendFut<Result<TxOutCompact, StoreError>>;
}

/// Shielded transaction indexing extension (Sapling + Orchard + commitment tree data).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_SHIELDED_EXT`].
pub trait BlockShieldedExt: Send + Sync {
    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<SaplingCompactTx>, StoreError>>;

    /// Fetch block sapling transaction data by height.
    fn get_block_sapling(&self, height: Height) -> impl SendFut<Result<SaplingTxList, StoreError>>;

    /// Fetches block sapling tx data for the given (inclusive) height range.
    fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<SaplingTxList>, StoreError>>;

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<OrchardCompactTx>, StoreError>>;

    /// Fetch block orchard transaction data by height.
    fn get_block_orchard(&self, height: Height) -> impl SendFut<Result<OrchardTxList, StoreError>>;

    /// Fetches block orchard tx data for the given (inclusive) height range.
    fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<OrchardTxList>, StoreError>>;

    /// Fetch the serialized Ironwood (NU6.3) compact tx for the given TxLocation, if present.
    ///
    /// Ironwood actions are modelled with the Orchard compact types. Returns `None` when the block
    /// has no ironwood row (any block below NU6.3 activation, or written before schema v1.3.0).
    fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<OrchardCompactTx>, StoreError>>;

    /// Fetch block ironwood transaction data by height.
    ///
    /// Returns an empty [`OrchardTxList`] when the block has no ironwood row.
    fn get_block_ironwood(&self, height: Height)
        -> impl SendFut<Result<OrchardTxList, StoreError>>;

    /// Fetches block ironwood tx data for the given (inclusive) height range.
    ///
    /// Heights with no ironwood row yield an empty [`OrchardTxList`].
    fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<OrchardTxList>, StoreError>>;

    /// Fetch block commitment tree data by height.
    fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> impl SendFut<Result<CommitmentTreeData, StoreError>>;

    /// Fetches block commitment tree data for the given (inclusive) height range.
    fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<CommitmentTreeData>, StoreError>>;
}

/// CompactBlock materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::COMPACT_BLOCK_EXT`].
pub trait CompactBlockExt: Send + Sync {
    /// Returns the compact block at `height`.
    ///
    /// A domain block, not a wire message. It is *dense*: one entry per
    /// transaction in the block, including transactions with nothing in any
    /// requested pool, so a transaction's position in the result is its
    /// position in the block. The wire form omits the empty ones, and the
    /// conversion to it does that — see
    /// [`compact_block_to_wire`](crate::conversion::compact_block_to_wire).
    ///
    /// The filter is still pushed down: a pool it excludes is not read from
    /// disk at all, which is where the saving is.
    fn get_compact_block(
        &self,
        height: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> impl SendFut<Result<zaino_primitives::types::CompactBlock, StoreError>>;

    /// Returns every compact block in `start..=end`, ascending.
    ///
    /// The range primitive: a backend answers under one read transaction, so
    /// the blocks are coherent with each other and the per-block transaction
    /// cost is paid once. A missing height is an error, not a skip.
    fn get_compact_block_range(
        &self,
        start: Height,
        end: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> impl SendFut<Result<Vec<zaino_primitives::types::CompactBlock>, StoreError>>;

    fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> impl SendFut<Result<CompactBlockStream, StoreError>>;
}

/// `IndexedBlock` materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::CHAIN_BLOCK_EXT`].
pub trait IndexedBlockExt: Send + Sync {
    /// Returns the [`IndexedBlock`] for `height`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(block))` if present,
    /// - `Ok(None)` if not present (not an error).
    ///
    fn get_chain_block(
        &self,
        height: Height,
    ) -> impl SendFut<Result<Option<IndexedBlock>, StoreError>>;

    /// Returns every [`IndexedBlock`] in `start..=end`, ascending.
    ///
    /// The range primitive, and the reason there is no batching helper built on
    /// [`Self::get_chain_block`]: a backend answers a range under one read
    /// transaction, so the blocks are coherent with each other, and the
    /// per-block transaction and validation costs are paid once.
    ///
    /// A missing height in the middle of the range is an error. The finalised
    /// state is contiguous, so a hole means corruption rather than a branch,
    /// and returning a short range would look to a caller like the chain ends
    /// there.
    fn get_chain_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<IndexedBlock>, StoreError>>;
}

/// One unspent output found by an address-history range query: where the
/// transaction sits, which output of it, and its value.
///
/// A named alias rather than a bare tuple repeated at five signatures — the
/// positions are not self-describing, and a `u16` beside a `u64` invites being
/// swapped.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) type AddrUtxo = (TxLocation, u16, u64);

/// Transparent address history indexing extension.
///
/// This extension provides address-scoped queries backed by persisted indices built from the
/// transparent transaction graph (outputs, spends, and derived address events).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::TRANSPARENT_HIST_INDEX`].
///
/// Range semantics:
/// - Methods that accept `start_height` and `end_height` interpret the range as inclusive:
///   `[start_height, end_height]`
// `pub(crate)`, unlike its eight sibling traits, because two of its methods
// return `AddrEventBytes`, which is `pub(crate)`. Narrowing the trait is the
// direction that keeps the packed 17-byte record private; widening the record
// to satisfy a `pub` the module never exports would leak an on-disk detail for
// nothing. The module itself is `pub(crate)` and none of these traits are
// re-exported, so this costs no reachability.
//
// Gated as a whole rather than per method: every method it has left is behind
// the feature, so without it the trait had no methods at all.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) trait TransparentHistExt: Send + Sync {
    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> impl SendFut<Result<Option<Vec<AddrEventBytes>>, StoreError>>;

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<Vec<AddrEventBytes>>, StoreError>>;

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<Option<Vec<TxLocation>>, StoreError>>;

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<Option<Vec<AddrUtxo>>, StoreError>>;

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<i64, StoreError>>;

    // TODO: Add addr_deltas_by_range method!
}

/// Spent-output indexing extension.
///
/// Answers which transaction spent a given outpoint. Built unconditionally from
/// schema v1.2 onward.
///
/// Its own trait, not part of [`TransparentHistExt`]. The two were one surface,
/// which meant a build with address history compiled out still had to advertise
/// an address-history capability in order to answer a spend lookup — a name that
/// described neither what was being asked nor what was built.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::SPENT_OUTPUT_INDEX`].
pub trait SpentOutputExt: Send + Sync {
    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> impl SendFut<Result<Option<TxLocation>, StoreError>>;

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> impl SendFut<Result<Vec<Option<TxLocation>>, StoreError>>;
}

/// UTXO-set accumulator extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::TXOUT_SET_INDEX`].
pub trait TxOutSetExt: Send + Sync {
    /// Returns the finalised-state txout-set accumulator.
    ///
    /// This is the finalised database portion of `gettxoutsetinfo`. It only contains values that
    /// are maintained by the finalised state:
    /// - number of transactions with at least one currently unspent transparent output;
    /// - number of currently unspent transparent outputs.
    ///
    /// Full RPC assembly, including non-finalised state and RPC-only fields, belongs above the
    /// finalised database layer.
    fn get_tx_out_set_info_accumulator(
        &self,
    ) -> impl SendFut<Result<FinalisedTxOutSetInfoAccumulator, StoreError>>;
}

#[cfg(test)]
mod tests {
    //! Tests for the schema-version → capability mapping.

    use super::{Capability, DbVersion};
    use crate::store::finalised_source::v1::DB_VERSION_V1;

    /// The current schema version must map to a capability set, not fall
    /// through to `empty()`.
    ///
    /// This is the guard the mapping was missing. `DB_VERSION_V1` was bumped to
    /// 1.3.0 for Ironwood without a matching arm here, so the current schema
    /// answered `Capability::empty()` — "this build understands nothing about
    /// this database". Nothing calls [`DbVersion::capability`] today, which is
    /// the only reason that was harmless; the moment routing consults it, an
    /// unmapped current version refuses every read against a perfectly good
    /// database.
    ///
    /// Bumping `DB_VERSION_V1` without extending the mapping fails here.
    #[test]
    fn the_current_schema_version_is_mapped() {
        assert_ne!(
            DB_VERSION_V1.capability(),
            Capability::empty(),
            "DB_VERSION_V1 is {DB_VERSION_V1} but `DbVersion::capability` has no arm for it, so it \
             falls through to the unknown-version case. Add an arm for this version."
        );
        assert_eq!(
            DB_VERSION_V1.capability(),
            Capability::LATEST,
            "the current schema backs every capability this build knows about, so its mapping and \
             `Capability::LATEST` must agree. If a new version genuinely adds a capability, add the \
             bit to both."
        );
    }

    /// A version this build has never heard of must yield nothing.
    ///
    /// Failing closed is the whole safety property of the mapping: a database
    /// written by a newer Zaino must be refused rather than read with this
    /// build's assumptions about its layout.
    #[test]
    fn an_unknown_schema_version_grants_nothing() {
        assert_eq!(
            DbVersion::new(2, 0, 0).capability(),
            Capability::empty(),
            "a future major version must fail closed"
        );
        assert_eq!(
            DbVersion::new(1, 99, 0).capability(),
            Capability::empty(),
            "an unrecognised minor version must fail closed"
        );
    }

    /// The spent index and the txout-set accumulator are not address history.
    ///
    /// This is the split's whole point. Both were reached through
    /// `TRANSPARENT_HIST_EXT`, so a production build — which does not enable
    /// `transparent_address_history_experimental` — had to advertise an
    /// address-history capability in order to answer `getspentinfo` or
    /// `gettxoutsetinfo`. The bit said "this database indexes addresses", the
    /// build could not do that, and it was true anyway for the thing actually
    /// being asked.
    #[test]
    fn v1_2_serves_the_transparent_indexes_whatever_the_build() {
        let capability = DbVersion::new(1, 2, 0).capability();

        assert!(capability.has(Capability::SPENT_OUTPUT_INDEX));
        assert!(capability.has(Capability::TXOUT_SET_INDEX));

        // Address history tracks the feature, and only the feature.
        assert_eq!(
            capability.has(Capability::TRANSPARENT_HIST_INDEX),
            cfg!(feature = "transparent_address_history_experimental"),
        );
    }

    /// A v1.0 or v1.1 database built without address history has no spent index.
    ///
    /// The behaviour change the split makes visible, and the reason it needs its
    /// own test. Before v1.2 the spent index was built *only* under the
    /// address-history feature, so a database of that vintage from a production
    /// build genuinely does not have those rows. The old mapping advertised
    /// `TRANSPARENT_HIST_EXT` for it regardless, which meant routing would send
    /// a spend lookup to a backend with nothing to look up in.
    ///
    /// This is a partial-migration hazard, not a theoretical one: a v1.0
    /// database is exactly what a node that has not yet migrated is holding.
    #[test]
    fn a_pre_v1_2_database_has_the_transparent_indexes_only_with_the_feature() {
        let with_feature = cfg!(feature = "transparent_address_history_experimental");

        for version in [DbVersion::new(1, 0, 0), DbVersion::new(1, 1, 0)] {
            let capability = version.capability();

            assert_eq!(capability.has(Capability::SPENT_OUTPUT_INDEX), with_feature);
            assert_eq!(capability.has(Capability::TXOUT_SET_INDEX), with_feature);
            assert_eq!(
                capability.has(Capability::TRANSPARENT_HIST_INDEX),
                with_feature,
            );

            // The block surfaces are there either way — the split changed
            // nothing about what a pre-v1.2 database can say about blocks.
            assert!(capability.has(Capability::READ_CORE));
            assert!(capability.has(Capability::BLOCK_TRANSPARENT_EXT));
            assert!(capability.has(Capability::CHAIN_BLOCK_EXT));
        }
    }

    /// v1.3 shares v1.2's mapping deliberately — Ironwood added rows, not a
    /// capability. Pinned so a future edit cannot silently give one of them a
    /// different set.
    #[test]
    fn ironwood_did_not_change_the_capability_set() {
        assert_eq!(
            DbVersion::new(1, 3, 0).capability(),
            DbVersion::new(1, 2, 0).capability(),
        );
    }
}
