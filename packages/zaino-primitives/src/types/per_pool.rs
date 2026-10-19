//! One value per shielded pool

use zcash_protocol::ShieldedPool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerPool<T> {
    pub sapling: T,
    pub orchard: T,
    pub ironwood: T,
}

impl<T> PerPool<T> {
    pub fn get(&self, pool: ShieldedPool) -> &T {
        match pool {
            ShieldedPool::Sapling => &self.sapling,
            ShieldedPool::Orchard => &self.orchard,
            ShieldedPool::Ironwood => &self.ironwood,
        }
    }

    #[cfg(test)]
    pub(crate) fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerPool<U> {
        PerPool { sapling: f(self.sapling), orchard: f(self.orchard), ironwood: f(self.ironwood) }
    }
}
