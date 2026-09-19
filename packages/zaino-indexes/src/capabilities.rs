//! The `Capability ⇄ IndexId` relation, and serviceability derived from it.
//!
//! A capability is *backed* by a set of local indexes. A capability with an
//! empty set has no local index and is served by the validator (passthrough).
//! Serviceability is then **derived, not hand-kept**: a capability is answerable
//! up to the finalised tip exactly when every index that backs it is built. Add
//! an index to the set that backs a capability and that capability's
//! serviceability automatically depends on it.

use strum::IntoEnumIterator;
use zaino_core::{Capability, ServiceabilityManifest};
use zaino_primitives::types::{Height, IndexId};
use zaino_sync::backend::BackendReader;

use crate::indexes::{
    address_history, hash_to_height, headers, orchard, sapling, transparent_data,
    transparent_spends, txid_location, txids,
};

/// The local indexes that back `capability`.
///
/// Empty for a passthrough capability — no local index, the validator answers.
/// Exhaustive by design: a new [`Capability`] variant must be classified here,
/// which keeps this relation and `resolve`'s serving strategy in step (a
/// non-empty set is locally served; an empty set is passthrough).
pub fn capability_indexes(capability: Capability) -> &'static [IndexId] {
    match capability {
        // A compact block is composed from the granular per-pool indexes;
        // by-hash access adds the hash→height index.
        Capability::Blocks => &[
            headers::ID,
            txids::ID,
            transparent_data::ID,
            sapling::ID,
            orchard::ID,
            hash_to_height::ID,
        ],
        // Where a transaction was mined — a local lookup. Its raw bytes are a
        // *separate* capability (`RawTransaction`), served by the validator.
        Capability::TransactionLocation => &[txid_location::ID],
        Capability::AddressHistory => &[address_history::ID],
        Capability::SpendStatus => &[transparent_spends::ID],
        // No local index — served by the validator.
        Capability::RawTransaction
        | Capability::Treestate
        | Capability::SubtreeRoots
        | Capability::Mempool
        | Capability::Broadcast
        | Capability::ReportedUpgrades => &[],
    }
}

/// Whether `index` has been built into `reader`'s backend.
///
/// "Built" = the writer has stamped the index's format version. Format
/// *compatibility* is enforced elsewhere — the engine rejects an incompatible
/// index when it loads state — so presence is enough here. A backend read
/// failure reads as not-built, the safe direction.
fn index_built(reader: &dyn BackendReader, index: IndexId) -> bool {
    zaino_persistence_codec::recorded_version(reader, index.into())
        .map(|recorded| recorded.is_some())
        .unwrap_or(false)
}

/// Derive the serviceability manifest from the built index set.
///
/// For each capability: answerable up to `finalized_tip` when every index that
/// backs it is built, else `None`. A passthrough capability (no backing index)
/// is `None` here — it is not *locally* serviceable; the runtime answers it via
/// the validator, which is resolve's concern, not this manifest's.
pub fn serviceability(
    reader: &dyn BackendReader,
    finalized_tip: Option<Height>,
) -> ServiceabilityManifest {
    let answerable = Capability::iter()
        .map(|capability| {
            let required = capability_indexes(capability);
            let answerable_to = if !required.is_empty()
                && required.iter().all(|index| index_built(reader, *index))
            {
                finalized_tip
            } else {
                None
            };
            (capability, answerable_to)
        })
        .collect();
    ServiceabilityManifest { answerable }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::in_memory::InMemoryBackend;
    use zaino_persistence::{Backend, BackendWriter, WriteOp};
    use zaino_persistence_codec::version_stamp;

    use crate::indexes::address_history::AddressHistoryIndex;
    use crate::indexes::hash_to_height::HashToHeightIndex;
    use crate::indexes::headers::HeadersIndex;
    use crate::indexes::orchard::OrchardIndex;
    use crate::indexes::sapling::SaplingIndex;
    use crate::indexes::transparent_data::TransparentDataIndex;
    use crate::indexes::transparent_spends::TransparentSpendsIndex;
    use crate::indexes::txids::TxidsIndex;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn answerable(manifest: &ServiceabilityManifest, capability: Capability) -> Option<Height> {
        manifest
            .answerable
            .iter()
            .find(|(cap, _)| *cap == capability)
            .and_then(|(_, height)| *height)
    }

    fn commit(backend: &InMemoryBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
    }

    #[test]
    fn passthrough_capabilities_have_no_backing_index() {
        assert!(capability_indexes(Capability::Treestate).is_empty());
        assert!(capability_indexes(Capability::Broadcast).is_empty());
        assert!(!capability_indexes(Capability::Blocks).is_empty());
    }

    #[test]
    fn a_fresh_backend_serves_nothing_locally() {
        let backend = InMemoryBackend::new();
        let reader = backend.reader().expect("reader");
        let manifest = serviceability(&reader, None);
        for (_, height) in &manifest.answerable {
            assert_eq!(*height, None);
        }
    }

    #[test]
    fn a_capability_is_serviceable_only_when_all_its_indexes_are_built() {
        let backend = InMemoryBackend::new();
        // Build the block indexes but *not* the address / spend indexes.
        commit(
            &backend,
            vec![
                version_stamp::<HeadersIndex>(headers::ID.into()),
                version_stamp::<TxidsIndex>(txids::ID.into()),
                version_stamp::<TransparentDataIndex>(transparent_data::ID.into()),
                version_stamp::<SaplingIndex>(sapling::ID.into()),
                version_stamp::<OrchardIndex>(orchard::ID.into()),
                version_stamp::<HashToHeightIndex>(hash_to_height::ID.into()),
            ],
        );
        let reader = backend.reader().expect("reader");
        let manifest = serviceability(&reader, Some(height(100)));

        // Blocks: all backing indexes built → answerable to the tip.
        assert_eq!(answerable(&manifest, Capability::Blocks), Some(height(100)));
        // AddressHistory / SpendStatus: their index is missing → not answerable.
        assert_eq!(answerable(&manifest, Capability::AddressHistory), None);
        assert_eq!(answerable(&manifest, Capability::SpendStatus), None);

        // Now build address_history and spends too.
        commit(
            &backend,
            vec![
                version_stamp::<AddressHistoryIndex>(address_history::ID.into()),
                version_stamp::<TransparentSpendsIndex>(transparent_spends::ID.into()),
            ],
        );
        let reader = backend.reader().expect("reader");
        let manifest = serviceability(&reader, Some(height(100)));
        assert_eq!(
            answerable(&manifest, Capability::AddressHistory),
            Some(height(100))
        );
        assert_eq!(
            answerable(&manifest, Capability::SpendStatus),
            Some(height(100))
        );
    }
}
