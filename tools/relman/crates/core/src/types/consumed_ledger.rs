use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{CycleId, InvalidCycleId, InvalidUid, Uid};

/// The consumed-UID ledger: the set of changeset [`Uid`]s that a past release has
/// already shipped.
///
/// A changeset gets stamped `consumed_in = "cycle-N"` per file at release, but
/// that per-file mark only reaches `dev` through the stable→dev backport. Until
/// it does, a derivation running on `dev` would re-count an already-shipped
/// changeset. The ledger closes that gap: it is a single file, refreshed from
/// `origin/stable` before each derivation, that lists every shipped changeset by
/// its immutable id. Derivation then excludes a changeset either by its per-file
/// mark *or* by its presence here — whichever arrives first — so dedup no longer
/// depends on the backport having landed.
///
/// Keyed by [`Uid`] so [`contains`](ConsumedLedger::contains) is a set-membership
/// test and [`insert`](ConsumedLedger::insert) is idempotent on a repeated id.
/// The `BTreeMap` also fixes a deterministic (id-sorted) iteration and
/// serialization order, so [`to_toml`](ConsumedLedger::to_toml) round-trips
/// stably regardless of insertion order.
///
/// An absent or empty ledger file is an *empty* ledger, never an error — but
/// that emptiness is a store/caller concern (a missing file yields
/// [`ConsumedLedger::default`]), not a parsing one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsumedLedger {
    entries: BTreeMap<Uid, ConsumedEntry>,
}

/// One ledger row: a shipped changeset's immutable [`id`](ConsumedEntry::id), the
/// [`cycle`](ConsumedEntry::cycle) that shipped it, and an optional human-audit
/// [`slug`](ConsumedEntry::slug).
///
/// The id is the load-bearing key — dedup turns on it. The slug is carried purely
/// so a human reading the ledger can tell which PR a row came from; nothing keys
/// on it, and a row remains valid without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedEntry {
    id: Uid,
    cycle: CycleId,
    slug: Option<String>,
}

impl ConsumedEntry {
    /// The shipped changeset's immutable identity — the ledger's dedup key.
    pub fn id(&self) -> &Uid {
        &self.id
    }

    /// The cycle that consumed (shipped) this changeset.
    pub fn cycle(&self) -> &CycleId {
        &self.cycle
    }

    /// The human-audit slug the changeset carried when consumed, if recorded.
    pub fn slug(&self) -> Option<&str> {
        self.slug.as_deref()
    }
}

/// Everything that can go wrong parsing a [`ConsumedLedger`] from TOML.
///
/// Parse-don't-validate: a malformed document or a bad `id`/`cycle` fails here,
/// so a holder of a `ConsumedLedger` never re-checks its rows.
#[derive(Debug, thiserror::Error)]
pub enum ConsumedLedgerError {
    /// The bytes were not valid TOML for the expected schema.
    #[error("failed to parse consumed-ledger TOML")]
    Toml(#[source] toml::de::Error),
    /// A row's `id` was present but not a valid [`Uid`].
    #[error("invalid id {value:?} in consumed-ledger entry")]
    InvalidUid {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidUid,
    },
    /// A row's `cycle` was present but not a valid [`CycleId`].
    #[error("invalid cycle {value:?} in consumed-ledger entry")]
    InvalidCycle {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCycleId,
    },
}

impl ConsumedLedger {
    /// Parse a ledger from its TOML representation, validating every row's `id`
    /// through [`Uid`] and `cycle` through [`CycleId`]. An empty document (no
    /// `[[consumed]]` rows) yields an empty ledger. A repeated id keeps the first
    /// row, mirroring [`insert`](ConsumedLedger::insert)'s idempotence.
    pub fn parse_toml(input: &str) -> Result<Self, ConsumedLedgerError> {
        let raw: RawLedger = toml::from_str(input).map_err(ConsumedLedgerError::Toml)?;
        let mut ledger = Self::default();
        for row in raw.consumed {
            let id = Uid::parse(&row.id).map_err(|source| ConsumedLedgerError::InvalidUid {
                value: row.id.clone(),
                source,
            })?;
            let cycle =
                CycleId::parse(&row.cycle).map_err(|source| ConsumedLedgerError::InvalidCycle {
                    value: row.cycle.clone(),
                    source,
                })?;
            ledger.insert(id, cycle, row.slug);
        }
        Ok(ledger)
    }

    /// Serialize back to TOML such that [`parse_toml`](ConsumedLedger::parse_toml)
    /// round-trips to `self`. Rows are emitted id-sorted (the map's order), so the
    /// output is stable under reordering of the inserts that built it.
    pub fn to_toml(&self) -> String {
        let raw = RawLedger {
            consumed: self
                .entries
                .values()
                .map(|entry| RawEntry {
                    id: entry.id.as_str().to_owned(),
                    cycle: entry.cycle.as_str().to_owned(),
                    slug: entry.slug.clone(),
                })
                .collect(),
        };
        // Serializing our own mirror of a fixed schema cannot fail: every field
        // is a plain string with no map-key or datetime hazard.
        toml::to_string(&raw).expect("consumed-ledger mirror is always serializable")
    }

    /// Whether `id` has already been shipped (is present in the ledger).
    pub fn contains(&self, id: &Uid) -> bool {
        self.entries.contains_key(id)
    }

    /// Record `id` as shipped by `cycle`, with an optional audit `slug`.
    ///
    /// Idempotent: a repeated id keeps the existing row unchanged, so re-consuming
    /// an already-ledgered changeset never duplicates or rewrites its entry.
    pub fn insert(&mut self, id: Uid, cycle: CycleId, slug: Option<String>) {
        self.entries
            .entry(id.clone())
            .or_insert(ConsumedEntry { id, cycle, slug });
    }

    /// Iterate the rows in id-sorted order.
    pub fn entries(&self) -> impl Iterator<Item = &ConsumedEntry> {
        self.entries.values()
    }

    /// Whether the ledger has no rows.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many changesets the ledger records as shipped.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The ledger document, mirrored for serde. `[[consumed]]` in TOML deserializes
/// as the `consumed` array; an absent array defaults to empty.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawLedger {
    #[serde(default)]
    consumed: Vec<RawEntry>,
}

/// One `[[consumed]]` row, mirrored for serde. Field order (`id`, `cycle`,
/// `slug`) is the on-disk order.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    id: String,
    cycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(raw: &str) -> Uid {
        Uid::parse(raw).expect("valid test uid")
    }

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid test cycle id")
    }

    const UID_A: &str = "018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f";
    const UID_B: &str = "028f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f";

    const SAMPLE: &str = "\
[[consumed]]
id = \"018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f\"
cycle = \"cycle-1\"
slug = \"pr-9\"
";

    #[test]
    fn empty_document_is_an_empty_ledger() {
        let ledger = ConsumedLedger::parse_toml("").expect("empty parses");
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn round_trips_through_toml() {
        let ledger = ConsumedLedger::parse_toml(SAMPLE).expect("parses");
        assert_eq!(ledger.len(), 1);
        assert!(ledger.contains(&uid(UID_A)));

        let reparsed = ConsumedLedger::parse_toml(&ledger.to_toml()).expect("reparses");
        assert_eq!(ledger, reparsed);

        let entry = reparsed.entries().next().expect("one row");
        assert_eq!(entry.id(), &uid(UID_A));
        assert_eq!(entry.cycle(), &cycle("cycle-1"));
        assert_eq!(entry.slug(), Some("pr-9"));
    }

    #[test]
    fn insert_is_idempotent_on_a_duplicate_id() {
        let mut ledger = ConsumedLedger::default();
        ledger.insert(uid(UID_A), cycle("cycle-1"), Some("pr-9".to_owned()));
        // A second insert of the same id — even with a different cycle/slug —
        // keeps the first row and does not grow the ledger.
        ledger.insert(uid(UID_A), cycle("cycle-2"), None);
        assert_eq!(ledger.len(), 1);
        let entry = ledger.entries().next().expect("one row");
        assert_eq!(entry.cycle(), &cycle("cycle-1"));
        assert_eq!(entry.slug(), Some("pr-9"));
    }

    #[test]
    fn entries_are_id_sorted_regardless_of_insert_order() {
        let mut ledger = ConsumedLedger::default();
        ledger.insert(uid(UID_B), cycle("cycle-1"), None);
        ledger.insert(uid(UID_A), cycle("cycle-1"), None);
        let ids: Vec<&str> = ledger.entries().map(|e| e.id().as_str()).collect();
        assert_eq!(ids, [UID_A, UID_B]);
    }

    #[test]
    fn slug_is_optional_on_the_wire() {
        let input =
            "[[consumed]]\nid = \"018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f\"\ncycle = \"cycle-1\"\n";
        let ledger = ConsumedLedger::parse_toml(input).expect("parses without slug");
        assert_eq!(ledger.entries().next().expect("row").slug(), None);
        // And it survives a round-trip (the key is simply omitted).
        let reparsed = ConsumedLedger::parse_toml(&ledger.to_toml()).expect("reparses");
        assert_eq!(ledger, reparsed);
    }

    #[test]
    fn rejects_a_bad_id() {
        let input = "[[consumed]]\nid = \"not-a-uuid\"\ncycle = \"cycle-1\"\n";
        assert!(matches!(
            ConsumedLedger::parse_toml(input),
            Err(ConsumedLedgerError::InvalidUid { value, .. }) if value == "not-a-uuid"
        ));
    }

    #[test]
    fn rejects_a_bad_cycle() {
        let input =
            "[[consumed]]\nid = \"018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f\"\ncycle = \"Cycle_1\"\n";
        assert!(matches!(
            ConsumedLedger::parse_toml(input),
            Err(ConsumedLedgerError::InvalidCycle { value, .. }) if value == "Cycle_1"
        ));
    }
}
