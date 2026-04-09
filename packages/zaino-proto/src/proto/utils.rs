use crate::proto::{
    compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction},
    service::{BlockId, BlockRange, PoolType},
};
#[cfg(feature = "heavy")]
use zebra_chain::block::Height;
#[cfg(feature = "heavy")]
use zebra_state::HashOrHeight;

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
pub fn pool_types_from_vector(pool_types: &[i32]) -> Result<Vec<PoolType>, PoolTypeError> {
    let pools = if pool_types.is_empty() {
        vec![PoolType::Sapling, PoolType::Orchard]
    } else {
        let mut pools: Vec<PoolType> = vec![];

        for pool in pool_types.iter() {
            match PoolType::try_from(*pool) {
                Ok(pool_type) => {
                    if pool_type == PoolType::Invalid {
                        return Err(PoolTypeError::InvalidPoolType);
                    } else {
                        pools.push(pool_type);
                    }
                }
                Err(_) => {
                    return Err(PoolTypeError::UnknownPoolType(*pool));
                }
            };
        }

        pools.clone()
    };
    Ok(pools)
}

/// Converts a `Vec<Pooltype>` into a `Vec<i32>`
pub fn pool_types_into_i32_vec(pool_types: Vec<PoolType>) -> Vec<i32> {
    pool_types.iter().map(|p| *p as i32).collect()
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
/// - `pool_types` has been validated via `pool_types_from_vector`.
pub struct ValidatedBlockRangeRequest {
    start: u64,
    end: u64,
    pool_types: Vec<PoolType>,
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
        let start = match &request.start {
            Some(block_id) => block_id.height,
            None => {
                return Err(GetBlockRangeError::NoStartHeightProvided);
            }
        };
        let end = match &request.end {
            Some(block_id) => block_id.height,
            None => {
                return Err(GetBlockRangeError::NoEndHeightProvided);
            }
        };

        if u32::try_from(start).is_err() {
            return Err(GetBlockRangeError::StartHeightOutOfRange);
        }
        if u32::try_from(end).is_err() {
            return Err(GetBlockRangeError::EndHeightOutOfRange);
        }

        let pool_types = pool_types_from_vector(&request.pool_types)
            .map_err(GetBlockRangeError::PoolTypeArgumentError)?;

        Ok(ValidatedBlockRangeRequest {
            start,
            end,
            pool_types,
        })
    }

    /// Start Height of the BlockRange Request
    pub fn start(&self) -> u64 {
        self.start
    }

    /// End Height of the BlockRange Request
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Pool Types of the BlockRange request
    pub fn pool_types(&self) -> Vec<PoolType> {
        self.pool_types.clone()
    }

    /// checks whether this request is specified in reversed order
    pub fn is_reverse_ordered(&self) -> bool {
        self.start > self.end
    }

    /// Reverses the order of this request
    pub fn reverse(&mut self) {
        (self.start, self.end) = (self.end, self.start);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolTypeFilter {
    include_transparent: bool,
    include_sapling: bool,
    include_orchard: bool,
}

impl std::default::Default for PoolTypeFilter {
    /// By default PoolType includes `Sapling` and `Orchard` pools.
    fn default() -> Self {
        PoolTypeFilter {
            include_transparent: false,
            include_sapling: true,
            include_orchard: true,
        }
    }
}

impl PoolTypeFilter {
    /// A PoolType Filter that will include all existing pool types.
    pub fn includes_all() -> Self {
        PoolTypeFilter {
            include_transparent: true,
            include_sapling: true,
            include_orchard: true,
        }
    }

    /// create a `PoolTypeFilter` from a vector of raw i32 `PoolType`s
    /// If the vector is empty it will return `Self::default()`.
    /// If the vector contains `PoolType::Invalid` or the vector contains more than 3 elements
    /// returns `PoolTypeError::InvalidPoolType`
    pub fn new_from_slice(pool_types: &[i32]) -> Result<Self, PoolTypeError> {
        let pool_types = pool_types_from_vector(pool_types)?;

        Self::new_from_pool_types(&pool_types)
    }

    /// create a `PoolTypeFilter` from a vector of `PoolType`
    /// If the vector is empty it will return `Self::default()`.
    /// If the vector contains `PoolType::Invalid` or the vector contains more than 3 elements
    /// returns `PoolTypeError::InvalidPoolType`
    pub fn new_from_pool_types(
        pool_types: &Vec<PoolType>,
    ) -> Result<PoolTypeFilter, PoolTypeError> {
        if pool_types.len() > PoolType::Orchard as usize {
            return Err(PoolTypeError::InvalidPoolType);
        }

        if pool_types.is_empty() {
            Ok(Self::default())
        } else {
            let mut filter = PoolTypeFilter::empty();

            for pool_type in pool_types {
                match pool_type {
                    PoolType::Invalid => return Err(PoolTypeError::InvalidPoolType),
                    PoolType::Transparent => filter.include_transparent = true,
                    PoolType::Sapling => filter.include_sapling = true,
                    PoolType::Orchard => filter.include_orchard = true,
                }
            }

            // guard against returning an invalid state this shouls never happen.
            if filter.is_empty() {
                Ok(Self::default())
            } else {
                Ok(filter)
            }
        }
    }

    /// only internal use. this in an invalid state.
    fn empty() -> Self {
        Self {
            include_transparent: false,
            include_sapling: false,
            include_orchard: false,
        }
    }

    /// only internal use
    fn is_empty(&self) -> bool {
        !self.include_transparent && !self.include_sapling && !self.include_orchard
    }

    /// retuns whether the filter includes transparent data
    pub fn includes_transparent(&self) -> bool {
        self.include_transparent
    }

    /// returns whether the filter includes orchard data
    pub fn includes_sapling(&self) -> bool {
        self.include_sapling
    }

    // returnw whether the filter includes orchard data
    pub fn includes_orchard(&self) -> bool {
        self.include_orchard
    }

    /// Convert this filter into the corresponding `Vec<PoolType>`.
    ///
    /// The resulting vector contains each included pool type at most once.
    pub fn to_pool_types_vector(&self) -> Vec<PoolType> {
        let mut pool_types: Vec<PoolType> = Vec::new();

        if self.include_transparent {
            pool_types.push(PoolType::Transparent);
        }

        if self.include_sapling {
            pool_types.push(PoolType::Sapling);
        }

        if self.include_orchard {
            pool_types.push(PoolType::Orchard);
        }

        pool_types
    }

    /// testing only
    #[allow(dead_code)]
    pub(crate) fn from_checked_parts(
        include_transparent: bool,
        include_sapling: bool,
        include_orchard: bool,
    ) -> Self {
        PoolTypeFilter {
            include_transparent,
            include_sapling,
            include_orchard,
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

/// prunes a compact block from transaction in formation related to pools not included in the
/// `pool_types` vector.
/// Note: for backwards compatibility an empty vector will return Sapling and Orchard Tx info.
pub fn compact_block_with_pool_types(
    mut block: CompactBlock,
    pool_types: &[PoolType],
) -> CompactBlock {
    if pool_types.is_empty() {
        for compact_tx in &mut block.vtx {
            // strip out transparent inputs if not Requested
            compact_tx.vin.clear();
            compact_tx.vout.clear();
        }

        // Omit transactions that have no Sapling/Orchard elements.
        block.vtx.retain(|compact_tx| {
            !compact_tx.spends.is_empty()
                || !compact_tx.outputs.is_empty()
                || !compact_tx.actions.is_empty()
        });
    } else {
        for compact_tx in &mut block.vtx {
            // strip out transparent inputs if not Requested
            if !pool_types.contains(&PoolType::Transparent) {
                compact_tx.vin.clear();
                compact_tx.vout.clear();
            }
            // strip out sapling if not requested
            if !pool_types.contains(&PoolType::Sapling) {
                compact_tx.spends.clear();
                compact_tx.outputs.clear();
            }
            // strip out orchard if not requested
            if !pool_types.contains(&PoolType::Orchard) {
                compact_tx.actions.clear();
            }
        }

        // Omit transactions that have no elements in any requested pool type.
        block.vtx.retain(|compact_tx| {
            !compact_tx.vin.is_empty()
                || !compact_tx.vout.is_empty()
                || !compact_tx.spends.is_empty()
                || !compact_tx.outputs.is_empty()
                || !compact_tx.actions.is_empty()
        });
    }

    block
}

/// Strips the ouputs and from all transactions, retains only
/// the nullifier from all orcard actions, and clears the chain
/// metadata from the block
pub fn compact_block_to_nullifiers(mut block: CompactBlock) -> CompactBlock {
    for ctransaction in &mut block.vtx {
        ctransaction.outputs = Vec::new();
        for caction in &mut ctransaction.actions {
            *caction = CompactOrchardAction {
                nullifier: caction.nullifier.clone(),
                ..Default::default()
            }
        }
    }

    block.chain_metadata = Some(ChainMetadata {
        sapling_commitment_tree_size: 0,
        orchard_commitment_tree_size: 0,
    });
    block
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
        let pools = [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard].to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Ok(PoolTypeFilter::from_checked_parts(true, true, true))
        );
    }

    #[test]
    fn test_pool_type_filter_t() {
        let pools = [PoolType::Transparent].to_vec();

        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&pools),
            Ok(PoolTypeFilter::from_checked_parts(true, false, false))
        );
    }

    #[test]
    fn test_pool_type_filter_default() {
        assert_eq!(
            PoolTypeFilter::new_from_pool_types(&vec![]),
            Ok(PoolTypeFilter::default())
        );
    }

    #[test]
    fn test_pool_type_filter_includes_all() {
        assert_eq!(
            PoolTypeFilter::from_checked_parts(true, true, true),
            PoolTypeFilter::includes_all()
        );
    }
}
