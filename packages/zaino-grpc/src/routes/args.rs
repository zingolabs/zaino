//! Request-field validation shared by routes: heights, ranges, `poolTypes`, block hashes

use tonic::Status;
use zaino_index_compact_block::Pools;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

/// Client height → [`BlockHeight`] (past the protocol ceiling = the client's error, not ours)
pub(super) fn height(raw: u64, field: &str) -> Result<BlockHeight, Status> {
    BlockHeight::try_from(raw).map_err(|_| {
        Status::invalid_argument(format!("{field} {raw} is above the protocol height ceiling"))
    })
}

/// `BlockID.hash` → `(height, hash)` via `at`'s block-hash index, at or below `through` (the
/// answering index's [`zaino_nfs::IndexesAtBlock::answers_through`])
///
/// - height only: the answering index confirms it holds `hash` there (indexes fold one block at
///   a time, a test may pair views of different chains)
pub(super) fn locate<V: zaino_persistence::PinnedView>(
    at: &zaino_nfs::IndexesAtBlock<V>,
    through: BlockHeight,
    raw: &[u8],
    method: &str,
) -> Result<(BlockHeight, [u8; 32]), Status> {
    let hash: [u8; 32] =
        raw.try_into().map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
    let locator = at.views().block_hash().ok_or_else(|| {
        Status::unimplemented(format!(
            "{method} by hash resolves through the block-hash index, which is off"
        ))
    })?;
    let height = locator.height_of(&BlockHash(hash)).filter(|&height| height <= through);
    let missing = || Status::not_found("block hash is not in the index");
    Ok((height.ok_or_else(missing)?, hash))
}

/// Client `poolTypes` → [`Pools`] (empty = shielded default, unknown / `POOL_TYPE_INVALID` refused)
pub(super) fn pools(raw: &[i32]) -> Result<Pools, Status> {
    use zaino_proto::proto::service::PoolType;

    if raw.is_empty() {
        return Ok(Pools::default());
    }

    let mut pools = Pools { sapling: false, orchard: false, ironwood: false, transparent: false };
    for &value in raw {
        match PoolType::try_from(value) {
            Ok(PoolType::Transparent) => pools.transparent = true,
            Ok(PoolType::Sapling) => pools.sapling = true,
            Ok(PoolType::Orchard) => pools.orchard = true,
            Ok(PoolType::Ironwood) => pools.ironwood = true,
            Ok(PoolType::Invalid) | Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "poolTypes value {value} is not a pool \
                     (TRANSPARENT=1, SAPLING=2, ORCHARD=3, IRONWOOD=4)"
                )))
            }
        }
    }

    Ok(pools)
}

/// `(start, end)`, both inclusive, refused when reversed (services take ordered ranges)
pub(super) fn ordered(
    start: BlockHeight,
    end: BlockHeight,
) -> Result<(BlockHeight, BlockHeight), Status> {
    match start <= end {
        true => Ok((start, end)),
        false => Err(Status::invalid_argument(format!("range start {start} is above end {end}"))),
    }
}

#[cfg(test)]
mod tests {
    use zaino_index_compact_block::Pools;
    use zaino_proto::proto::service::PoolType;

    /// - `poolTypes` → pools served (pruning itself = compact-block's projection)
    /// - unknown refused
    /// - Empty != all: wire pins it to the legacy shielded set (no transparent)
    #[test]
    fn pool_types_name_the_pools_empty_means_shielded_only_and_unknown_is_refused() {
        let pools = |named: &[PoolType]| {
            let raw: Vec<i32> = named.iter().map(|pool| *pool as i32).collect();
            super::pools(&raw).expect("known pools")
        };

        for (raw, value) in [(vec![PoolType::Invalid as i32], 0), (vec![2, 99, 3], 99)] {
            let refused = super::pools(&raw).expect_err("names no pool");
            let expected = format!(
                "poolTypes value {value} is not a pool \
                 (TRANSPARENT=1, SAPLING=2, ORCHARD=3, IRONWOOD=4)"
            );
            let got = (refused.code(), refused.message());
            assert_eq!(got, (tonic::Code::InvalidArgument, expected.as_str()), "{raw:?}");
        }

        let none = Pools { sapling: false, orchard: false, ironwood: false, transparent: false };
        let every =
            [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard, PoolType::Ironwood];
        #[rustfmt::skip]
        let cases = [
            ("none named = shielded only", vec![],                                     Pools { transparent: false, ..Pools::ALL }),
            ("transparent alone",          vec![PoolType::Transparent],                Pools { transparent: true, ..none }),
            ("sapling + ironwood",         vec![PoolType::Sapling, PoolType::Ironwood], Pools { sapling: true, ironwood: true, ..none }),
            ("all four",                   every.to_vec(),                             Pools::ALL),
        ];
        for (case, named, served) in cases {
            assert_eq!(pools(&named), served, "{case}");
        }
    }
}
