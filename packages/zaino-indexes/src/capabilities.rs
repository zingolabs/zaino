//! The `Capability ⇄ IndexId` relation, and the finalised store's
//! serviceability derived from it.
//!
//! A capability is *backed* by a set of local indexes. A capability with an
//! empty set has no local index here and is answered, if at all, by another
//! provider (the validator, through a passthrough) — which this module does
//! not know about, so it reports it [`Answerable::Absent`] *locally*. The
//! composer that holds the other providers widens that answer.
//!
//! Serviceability is **derived, not hand-kept**: a capability is answerable up
//! to the finalised tip exactly when every index that backs it is built. Add an
//! index to the set that backs a capability and that capability's
//! serviceability automatically depends on it.
//!
//! ```text
//! local(cap, w) = ToHeight(w)  if backing(cap) ≠ ∅ ∧ backing(cap) ⊆ built ∧ w known
//!               | NotYet       if backing(cap) ≠ ∅ ∧ backing(cap) ⊆ built ∧ w unknown
//!               | Absent       otherwise
//! ```

use zaino_core::{Answerable, Capability, ServiceabilityManifest};
use zaino_primitives::types::{Height, IndexId};
use zaino_sync::backend::BackendReader;

pub mod local;

use local::LocalCapability;

/// The local indexes that back `capability`.
///
/// Empty for a capability with no local index. Exhaustive by design: a new
/// [`Capability`] variant must be classified here, either as one of the
/// [`local`] declarations or as having no local index. Each local arm reads
/// the list off the same declaration the store's read bounds on, so this
/// function adds no second copy of the relation — only the variant → type
/// mapping.
pub fn capability_indexes(capability: Capability) -> &'static [IndexId] {
    match capability {
        Capability::Blocks => local::Blocks::INDEXES,
        Capability::TransactionLocation => local::TransactionLocation::INDEXES,
        Capability::AddressHistory => local::AddressHistory::INDEXES,
        Capability::SpendStatus => local::SpendStatus::INDEXES,
        // No local index — answered, if at all, by another provider.
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

/// Derive the finalised store's serviceability manifest from the built index set.
///
/// For each capability: answerable up to `finalized_tip` when every index that
/// backs it is built; `NotYet` when built but no watermark has been committed;
/// `Absent` when an index is missing or the capability has no local index at
/// all. The last is *locally* absent — a composer holding a passthrough
/// provider widens it.
pub fn serviceability(
    reader: &dyn BackendReader,
    finalized_tip: Option<Height>,
) -> ServiceabilityManifest {
    ServiceabilityManifest::derive(|capability| {
        let required = capability_indexes(capability);
        let built =
            !required.is_empty() && required.iter().all(|index| index_built(reader, *index));
        match (built, finalized_tip) {
            (true, Some(tip)) => Answerable::ToHeight(tip),
            (true, None) => Answerable::NotYet,
            (false, _) => Answerable::Absent,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::in_memory::InMemoryBackend;
    use zaino_persistence::{Backend, BackendWriter, WriteOp};
    use zaino_persistence_codec::version_stamp;

    use crate::indexes::{
        address_history, chain_metadata, hash_to_height, headers, ironwood, orchard, sapling,
        transparent_data, transparent_spends, txids,
    };

    use crate::indexes::address_history::AddressHistoryIndex;
    use crate::indexes::chain_metadata::ChainMetadataIndex;
    use crate::indexes::hash_to_height::HashToHeightIndex;
    use crate::indexes::headers::HeadersIndex;
    use crate::indexes::ironwood::IronwoodIndex;
    use crate::indexes::orchard::OrchardIndex;
    use crate::indexes::sapling::SaplingIndex;
    use crate::indexes::transparent_data::TransparentDataIndex;
    use crate::indexes::transparent_spends::TransparentSpendsIndex;
    use crate::indexes::txids::TxidsIndex;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn answerable(manifest: &ServiceabilityManifest, capability: Capability) -> Answerable {
        manifest.get(capability)
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
        for (_, answer) in manifest.iter() {
            assert_eq!(answer, Answerable::Absent);
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
                version_stamp::<IronwoodIndex>(ironwood::ID.into()),
                version_stamp::<HashToHeightIndex>(hash_to_height::ID.into()),
                version_stamp::<ChainMetadataIndex>(chain_metadata::ID.into()),
            ],
        );
        let reader = backend.reader().expect("reader");
        let manifest = serviceability(&reader, Some(height(100)));

        // Blocks: all backing indexes built → answerable to the tip.
        assert_eq!(
            answerable(&manifest, Capability::Blocks),
            Answerable::ToHeight(height(100))
        );
        // AddressHistory / SpendStatus: their index is missing → absent locally.
        assert_eq!(
            answerable(&manifest, Capability::AddressHistory),
            Answerable::Absent
        );
        assert_eq!(
            answerable(&manifest, Capability::SpendStatus),
            Answerable::Absent
        );
        // Built, but no watermark committed yet: offered, not yet serviceable.
        assert_eq!(
            answerable(&serviceability(&reader, None), Capability::Blocks),
            Answerable::NotYet
        );

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
            Answerable::ToHeight(height(100))
        );
        assert_eq!(
            answerable(&manifest, Capability::SpendStatus),
            Answerable::ToHeight(height(100))
        );
    }
}
