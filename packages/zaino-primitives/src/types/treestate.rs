//! Commitment tree state (serialized trees).

/// Serialized commitment tree bytes for one pool.
///
/// Opaque byte blob. Interpretation (Sapling vs Orchard,
/// deserialization into tree structures) happens in consumer crates.
pub type TreeBytes = Vec<u8>;

/// Commitment tree state at a block: Sapling, Orchard and Ironwood trees.
///
/// Any pool may be absent if the block predates that pool's activation.
#[derive(Debug, Clone)]
pub struct Treestate {
    /// Serialized Sapling commitment tree, if active at this height.
    pub sapling: Option<TreeBytes>,
    /// Serialized Orchard commitment tree, if active at this height.
    pub orchard: Option<TreeBytes>,
    /// Serialized Ironwood commitment tree, if active at this height (NU6.3).
    pub ironwood: Option<TreeBytes>,
}
