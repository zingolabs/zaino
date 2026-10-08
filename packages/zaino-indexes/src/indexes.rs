//! Individual index definitions.
//!
//! Each module defines one index: its context projection, extraction,
//! merge, schema, and encoding.

pub mod address_history;
pub mod chain_metadata;
pub mod hash_to_height;
pub mod headers;
pub mod ironwood;
pub mod orchard;
pub mod sapling;
pub mod subtrees;
pub mod transparent_data;
pub mod transparent_spends;
pub mod tree_state;
pub mod txid_location;
pub mod txids;

#[cfg(test)]
mod key_order_tests {
    //! Pin each index's [`KEY_ORDER`](zaino_persistence_codec::EntryCodec::KEY_ORDER)
    //! claim. These are the storage facts the deferral machinery keys off: a
    //! height-led key is `WalkOrdered` (sorted-appendable), a hash-led key that
    //! lands on scattered leaf pages is `Scattered` (deferrable). A change here
    //! is a change to what the backend may defer, so it must be deliberate.
    use zaino_persistence_codec::{EntryCodec, KeyOrder};

    #[test]
    fn walk_ordered_indexes() {
        assert_eq!(
            super::headers::HeadersIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        assert_eq!(super::txids::TxidsIndex::KEY_ORDER, KeyOrder::WalkOrdered);
        assert_eq!(
            super::transparent_data::TransparentDataIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        assert_eq!(
            super::sapling::SaplingIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        assert_eq!(
            super::orchard::OrchardIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        assert_eq!(
            super::ironwood::IronwoodIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        assert_eq!(
            super::chain_metadata::ChainMetadataIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
        // The subtree-roots indexes key on the subtree index (u32 big-endian),
        // which is numeric/walk order; the codec is generic, so one pool pins all.
        assert_eq!(
            super::subtrees::SaplingSubtreesIndex::KEY_ORDER,
            KeyOrder::WalkOrdered
        );
    }

    #[test]
    fn scattered_indexes() {
        assert_eq!(
            super::hash_to_height::HashToHeightIndex::KEY_ORDER,
            KeyOrder::Scattered
        );
        assert_eq!(
            super::address_history::AddressHistoryIndex::KEY_ORDER,
            KeyOrder::Scattered
        );
        assert_eq!(
            super::transparent_spends::TransparentSpendsIndex::KEY_ORDER,
            KeyOrder::Scattered
        );
        assert_eq!(
            super::txid_location::TxidLocationIndex::KEY_ORDER,
            KeyOrder::Scattered
        );
    }
}
