use std::collections::BTreeSet;

use crate::proto::{
    compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx},
    service::{BlockId, BlockRange, PoolType},
};
#[cfg(feature = "heavy")]
use zebra_chain::block::Height;
#[cfg(feature = "heavy")]
use zebra_state::HashOrHeight;

/// Every pool a request may name — the `PoolType` variants minus `Invalid`.
const KNOWN_POOLS: [PoolType; 4] = [
    PoolType::Transparent,
    PoolType::Sapling,
    PoolType::Orchard,
    PoolType::Ironwood,
];

#[derive(Debug, PartialEq, Eq)]
/// Errors that can arise when mapping `PoolType` from an `i32` value.
pub enum PoolTypeError {
    /// Pool Type value was map to the enum `PoolType::Invalid`.
    InvalidPoolType,
    /// Pool Type value was mapped to value that can't be mapped to a known pool type.
    UnknownPoolType(i32),
}

/// Converts a vector of pool_types (i32) into its rich-type representation
/// Returns `PoolTypeError::InvalidPoolType` when invalid `pool_types` are found
/// or `PoolTypeError::UnknownPoolType` if unknown ones are found.
///
/// An empty vector means the client did not filter, so every shielded pool is
/// served — including Ironwood, which clients that predate the field simply
/// ignore as an unknown protobuf field. Backfilling only the pre-NU6.3 pools
/// here would serve blocks whose `chainMetadata.ironwoodCommitmentTreeSize`
/// counts commitments from actions the block omits; a scanning wallet sees
/// that as a tree-size discontinuity and treats it as a chain reorg.
///
/// The unfiltered pool set has exactly one definition:
/// [`PoolTypeFilter::default`]. This wire-decode path delegates to it so the
/// two cannot drift again (they had: the filter default gained Ironwood while
/// this backfill still listed only Sapling and Orchard).
pub fn pool_types_from_vector(pool_types: &[i32]) -> Result<Vec<PoolType>, PoolTypeError> {
    if pool_types.is_empty() {
        return Ok(PoolTypeFilter::default().to_pool_types_vector());
    }
    pool_types
        .iter()
        .map(|&raw| match PoolType::try_from(raw) {
            Ok(PoolType::Invalid) => Err(PoolTypeError::InvalidPoolType),
            Ok(pool_type) => Ok(pool_type),
            Err(_) => Err(PoolTypeError::UnknownPoolType(raw)),
        })
        .collect()
}

/// Converts a slice of `PoolType`s into the `Vec<i32>` wire representation.
pub fn pool_types_into_i32_vec(pool_types: &[PoolType]) -> Vec<i32> {
    pool_types.iter().map(|&p| p as i32).collect()
}

/// Errors that can be present in the request of the GetBlockRange RPC
pub enum GetBlockRangeError {
    /// Error: No start height given.
    NoStartHeightProvided,
    /// Error: No end height given.
    NoEndHeightProvided,
    /// Start height out of range. Failed to convert to u32.
    StartHeightOutOfRange,

    /// End height out of range. Failed to convert to u32.
    EndHeightOutOfRange,
    /// An invalid pool type request was provided.
    PoolTypeArgumentError(PoolTypeError),
}

/// `BlockRange` request that has been validated in terms of the semantics
/// of `GetBlockRange` RPC.
///
/// # Guarantees
///
/// - `start` and `end` were provided in the request.
/// - `start` and `end` are in the inclusive range `0..=u32::MAX`, so they can be
///   safely converted to `u32` (for example via `u32::try_from(...)`) without
///   failing.
/// - the requested pools have been validated into a [`PoolTypeFilter`].
pub struct ValidatedBlockRangeRequest {
    start: u64,
    end: u64,
    filter: PoolTypeFilter,
}

impl ValidatedBlockRangeRequest {
    /// Validates a `BlockRange` in terms of the `GetBlockRange` RPC.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - [`GetBlockRangeError::NoStartHeightProvided`] if `request.start` is `None`.
    /// - [`GetBlockRangeError::NoEndHeightProvided`] if `request.end` is `None`.
    /// - [`GetBlockRangeError::StartHeightOutOfRange`] if `start` does not fit in a `u32`.
    /// - [`GetBlockRangeError::EndHeightOutOfRange`] if `end` does not fit in a `u32`.
    /// - [`GetBlockRangeError::PoolTypeArgumentError`] if pool types are invalid.
    pub fn new_from_block_range(
        request: &BlockRange,
    ) -> Result<ValidatedBlockRangeRequest, GetBlockRangeError> {
        // Presence is checked for both endpoints before either range check so
        // a request failing both reports the same error it always has.
        let start = provided_height(&request.start, GetBlockRangeError::NoStartHeightProvided)?;
        let end = provided_height(&request.end, GetBlockRangeError::NoEndHeightProvided)?;

        fits_in_u32(start, GetBlockRangeError::StartHeightOutOfRange)?;
        fits_in_u32(end, GetBlockRangeError::EndHeightOutOfRange)?;

        let filter = PoolTypeFilter::new_from_slice(&request.pool_types)
            .map_err(GetBlockRangeError::PoolTypeArgumentError)?;

        Ok(ValidatedBlockRangeRequest { start, end, filter })
    }

    /// Start Height of the BlockRange Request
    pub fn start(&self) -> u64 {
        self.start
    }

    /// End Height of the BlockRange Request
    pub fn end(&self) -> u64 {
        self.end
    }

    /// The validated pool filter of the BlockRange request
    pub fn pool_type_filter(&self) -> &PoolTypeFilter {
        &self.filter
    }
}

/// The height of a range endpoint, or `missing` when the endpoint was not
/// provided in the request.
fn provided_height(
    endpoint: &Option<BlockId>,
    missing: GetBlockRangeError,
) -> Result<u64, GetBlockRangeError> {
    Ok(endpoint.as_ref().ok_or(missing)?.height)
}

/// Errors with `out_of_range` when `height` cannot be represented as a `u32`.
fn fits_in_u32(height: u64, out_of_range: GetBlockRangeError) -> Result<(), GetBlockRangeError> {
    u32::try_from(height).map(|_| ()).map_err(|_| out_of_range)
}

/// The set of pools a request asks to be served.
///
/// Internally a set keyed by [`PoolType`], so "which pools exist" is
/// recorded once, in the enum (plus [`KNOWN_POOLS`]) — not once per method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolTypeFilter {
    included: BTreeSet<PoolType>,
}

impl std::default::Default for PoolTypeFilter {
    /// The unfiltered pool set: every shielded pool, transparent excluded.
    fn default() -> Self {
        Self::containing(|pool| pool != PoolType::Transparent)
    }
}

impl PoolTypeFilter {
    /// A PoolType Filter that will include all existing pool types.
    pub fn includes_all() -> Self {
        Self::containing(|_| true)
    }

    /// The filter holding every known pool that satisfies `include`.
    fn containing(include: impl Fn(PoolType) -> bool) -> Self {
        PoolTypeFilter {
            included: KNOWN_POOLS.into_iter().filter(|&p| include(p)).collect(),
        }
    }

    /// create a `PoolTypeFilter` from a vector of raw i32 `PoolType`s
    /// If the vector is empty it will return `Self::default()`.
    /// If the vector contains `PoolType::Invalid` or the vector contains more
    /// elements than there are known pools, returns `PoolTypeError::InvalidPoolType`.
    pub fn new_from_slice(pool_types: &[i32]) -> Result<Self, PoolTypeError> {
        let pool_types = pool_types_from_vector(pool_types)?;

        Self::new_from_pool_types(&pool_types)
    }

    /// create a `PoolTypeFilter` from a slice of `PoolType`
    /// If the slice is empty it will return `Self::default()`.
    /// If the slice contains `PoolType::Invalid` or the slice contains more
    /// elements than there are known pools, returns `PoolTypeError::InvalidPoolType`.
    pub fn new_from_pool_types(pool_types: &[PoolType]) -> Result<PoolTypeFilter, PoolTypeError> {
        if pool_types.len() > KNOWN_POOLS.len() {
            return Err(PoolTypeError::InvalidPoolType);
        }

        if pool_types.is_empty() {
            return Ok(Self::default());
        }

        let mut included = BTreeSet::new();
        for &pool_type in pool_types {
            if pool_type == PoolType::Invalid {
                return Err(PoolTypeError::InvalidPoolType);
            }
            included.insert(pool_type);
        }
        Ok(PoolTypeFilter { included })
    }

    /// returns whether the filter includes transparent data
    pub fn includes_transparent(&self) -> bool {
        self.included.contains(&PoolType::Transparent)
    }

    /// returns whether the filter includes sapling data
    pub fn includes_sapling(&self) -> bool {
        self.included.contains(&PoolType::Sapling)
    }

    /// returns whether the filter includes orchard data
    pub fn includes_orchard(&self) -> bool {
        self.included.contains(&PoolType::Orchard)
    }

    /// returns whether the filter includes ironwood data
    pub fn includes_ironwood(&self) -> bool {
        self.included.contains(&PoolType::Ironwood)
    }

    /// Convert this filter into the corresponding `Vec<PoolType>`.
    ///
    /// The resulting vector contains each included pool type at most once,
    /// in `PoolType` declaration order.
    pub fn to_pool_types_vector(&self) -> Vec<PoolType> {
        self.included.iter().copied().collect()
    }

    /// testing only
    #[cfg(test)]
    fn from_checked_parts(
        include_transparent: bool,
        include_sapling: bool,
        include_orchard: bool,
        include_ironwood: bool,
    ) -> Self {
        let flags = [
            include_transparent,
            include_sapling,
            include_orchard,
            include_ironwood,
        ];
        PoolTypeFilter {
            included: KNOWN_POOLS
                .into_iter()
                .zip(flags)
                .filter_map(|(pool, included)| included.then_some(pool))
                .collect(),
        }
    }
}

#[cfg(feature = "heavy")]
/// Converts [`BlockId`] into [`HashOrHeight`] Zebra type
pub fn blockid_to_hashorheight(block_id: BlockId) -> Option<HashOrHeight> {
    <[u8; 32]>::try_from(block_id.hash)
        .map(zebra_chain::block::Hash)
        .map(HashOrHeight::from)
        .or_else(|_| {
            block_id
                .height
                .try_into()
                .map(|height| HashOrHeight::Height(Height(height)))
        })
        .ok()
}

impl CompactTx {
    /// Whether any per-pool field of this transaction is non-empty.
    pub fn has_pool_data(&self) -> bool {
        !self.vin.is_empty()
            || !self.vout.is_empty()
            || !self.spends.is_empty()
            || !self.outputs.is_empty()
            || !self.actions.is_empty()
            || !self.ironwood_actions.is_empty()
    }
}

/// Prunes a compact block of transaction information related to pools the
/// filter excludes, then omits transactions left with no pool data at all.
///
/// An unfiltered request is expressed as [`PoolTypeFilter::default`]; the
/// request-decode path ([`PoolTypeFilter::new_from_slice`]) produces it for
/// an empty `poolTypes` field.
pub fn prune_compact_block(mut block: CompactBlock, filter: &PoolTypeFilter) -> CompactBlock {
    for compact_tx in &mut block.vtx {
        // strip out transparent inputs if not requested
        if !filter.includes_transparent() {
            compact_tx.vin.clear();
            compact_tx.vout.clear();
        }
        // strip out sapling if not requested
        if !filter.includes_sapling() {
            compact_tx.spends.clear();
            compact_tx.outputs.clear();
        }
        // strip out orchard if not requested
        if !filter.includes_orchard() {
            compact_tx.actions.clear();
        }
        // strip out ironwood if not requested
        if !filter.includes_ironwood() {
            compact_tx.ironwood_actions.clear();
        }
    }

    // Omit transactions that have no elements in any requested pool type.
    block.vtx.retain(CompactTx::has_pool_data);

    block
}

/// Strips the outputs and transparent data from all transactions, retains
/// only the nullifier from all Orchard and Ironwood actions, and clears the
/// chain metadata from the block. Sapling spends are kept whole: the spend
/// itself is the nullifier carrier.
pub fn compact_block_to_nullifiers(mut block: CompactBlock) -> CompactBlock {
    for ctransaction in &mut block.vtx {
        ctransaction.outputs = Vec::new();
        ctransaction.vin = Vec::new();
        ctransaction.vout = Vec::new();
        retain_only_nullifiers(&mut ctransaction.actions);
        retain_only_nullifiers(&mut ctransaction.ironwood_actions);
    }

    block.chain_metadata = Some(ChainMetadata {
        sapling_commitment_tree_size: 0,
        orchard_commitment_tree_size: 0,
        ironwood_commitment_tree_size: 0,
    });
    block
}

/// Reduces each action to one carrying only its nullifier.
fn retain_only_nullifiers(actions: &mut [CompactOrchardAction]) {
    for action in actions {
        *action = CompactOrchardAction {
            nullifier: std::mem::take(&mut action.nullifier),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod test {
    use crate::proto::{
        service::PoolType,
        utils::{PoolTypeError, PoolTypeFilter},
    };

    #[test]
    fn test_pool_type_filter_fails_when_invalid() {
        let pools = [
            PoolType::Transparent,
            PoolType::Sapling,
            PoolType::Orchard,
            PoolType::Invalid,
        ]
        .to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Err(PoolTypeError::InvalidPoolType)
        );
    }

    #[test]
    fn test_pool_type_filter_fails_when_too_many_items() {
        let pools = [
            PoolType::Transparent,
            PoolType::Sapling,
            PoolType::Orchard,
            PoolType::Ironwood,
            PoolType::Orchard,
        ]
        .to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Err(PoolTypeError::InvalidPoolType)
        );
    }

    #[test]
    fn test_pool_type_filter_t_z_o() {
        let pools = [
            PoolType::Transparent,
            PoolType::Sapling,
            PoolType::Orchard,
            PoolType::Ironwood,
        ]
        .to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Ok(PoolTypeFilter::from_checked_parts(true, true, true, true))
        );
    }

    #[test]
    fn test_pool_type_filter_t() {
        let pools = [PoolType::Transparent].to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Ok(PoolTypeFilter::from_checked_parts(
                true, false, false, false
            ))
        );
    }

    #[test]
    fn test_pool_type_filter_default() {
        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&[]),
            Ok(PoolTypeFilter::default())
        );
    }

    #[test]
    fn test_pool_type_filter_includes_all() {
        assert_eq!(
            PoolTypeFilter::from_checked_parts(true, true, true, true),
            PoolTypeFilter::includes_all()
        );
    }

    /// Regression: an unfiltered request (empty `poolTypes`, what every
    /// pre-Ironwood client sends) must be served Ironwood actions. When the
    /// empty-vector backfill listed only the pre-NU6.3 shielded pools, the
    /// served compact blocks stripped `ironwoodActions` while
    /// `chainMetadata.ironwoodCommitmentTreeSize` still counted them, and
    /// scanning wallets reported a tree-size discontinuity (a phantom chain
    /// reorg) at the first block with an Ironwood coinbase.
    #[test]
    fn empty_pool_types_request_includes_ironwood() {
        let pools = crate::proto::utils::pool_types_from_vector(&[]).unwrap();
        assert!(pools.contains(&PoolType::Ironwood), "{pools:?}");

        let filter = PoolTypeFilter::new_from_slice(&[]).unwrap();
        assert!(filter.includes_ironwood());
        assert!(filter.includes_sapling());
        assert!(filter.includes_orchard());
        assert!(!filter.includes_transparent());
    }
}
