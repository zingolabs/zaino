use std::collections::BTreeSet;

use crate::proto::{compact_formats::CompactTx, service::PoolType};

/// Every pool a request may name (`PoolType` minus `Invalid`)
const KNOWN_POOLS: [PoolType; 4] =
    [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard, PoolType::Ironwood];

/// Pools a request asks to be served
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolTypeFilter {
    included: BTreeSet<PoolType>,
}

impl std::default::Default for PoolTypeFilter {
    /// Unfiltered request (empty `poolTypes`): every shielded pool incl. Ironwood, no transparent
    fn default() -> Self {
        Self::containing(|pool| pool != PoolType::Transparent)
    }
}

impl PoolTypeFilter {
    /// Every known pool, transparent included
    pub fn includes_all() -> Self {
        Self::containing(|_| true)
    }

    fn containing(include: impl Fn(PoolType) -> bool) -> Self {
        PoolTypeFilter { included: KNOWN_POOLS.into_iter().filter(|&p| include(p)).collect() }
    }

    /// Transparent data served?
    pub fn includes_transparent(&self) -> bool {
        self.included.contains(&PoolType::Transparent)
    }

    /// Sapling data served?
    pub fn includes_sapling(&self) -> bool {
        self.included.contains(&PoolType::Sapling)
    }

    /// Orchard data served?
    pub fn includes_orchard(&self) -> bool {
        self.included.contains(&PoolType::Orchard)
    }

    /// Ironwood data served?
    pub fn includes_ironwood(&self) -> bool {
        self.included.contains(&PoolType::Ironwood)
    }
}

impl CompactTx {
    /// Any per-pool field non-empty?
    pub fn has_pool_data(&self) -> bool {
        !self.vin.is_empty()
            || !self.vout.is_empty()
            || !self.spends.is_empty()
            || !self.outputs.is_empty()
            || !self.actions.is_empty()
            || !self.ironwood_actions.is_empty()
    }
}

#[cfg(test)]
mod test {
    use super::PoolTypeFilter;

    /// Ironwood in the default set (omitting it desyncs `ironwoodCommitmentTreeSize` → phantom
    /// reorg in scanning wallets)
    #[test]
    fn default_serves_every_shielded_pool_and_all_adds_transparent() {
        let default = PoolTypeFilter::default();
        let all = PoolTypeFilter::includes_all();
        let flags = |f: &PoolTypeFilter| {
            [
                f.includes_transparent(),
                f.includes_sapling(),
                f.includes_orchard(),
                f.includes_ironwood(),
            ]
        };

        assert_eq!(flags(&default), [false, true, true, true], "default = shielded only");
        assert_eq!(flags(&all), [true, true, true, true], "includes_all = every pool");
    }
}
