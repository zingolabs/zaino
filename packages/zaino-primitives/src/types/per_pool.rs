//! One value per shielded pool

use zcash_protocol::ShieldedPool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerPool<T> {
    pub sapling: T,
    pub orchard: T,
    pub ironwood: T,
}

impl<T> PerPool<T> {
    pub fn from_fn(mut f: impl FnMut(ShieldedPool) -> T) -> Self {
        let [sapling, orchard, ironwood] = ShieldedPool::ALL.map(&mut f);
        Self { sapling, orchard, ironwood }
    }

    pub fn get(&self, pool: ShieldedPool) -> &T {
        match pool {
            ShieldedPool::Sapling => &self.sapling,
            ShieldedPool::Orchard => &self.orchard,
            ShieldedPool::Ironwood => &self.ironwood,
        }
    }

    pub fn get_mut(&mut self, pool: ShieldedPool) -> &mut T {
        match pool {
            ShieldedPool::Sapling => &mut self.sapling,
            ShieldedPool::Orchard => &mut self.orchard,
            ShieldedPool::Ironwood => &mut self.ironwood,
        }
    }

    /// [`ShieldedPool::ALL`] order
    pub fn iter(&self) -> impl Iterator<Item = (ShieldedPool, &T)> {
        ShieldedPool::ALL.into_iter().map(|pool| (pool, self.get(pool)))
    }

    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerPool<U> {
        PerPool { sapling: f(self.sapling), orchard: f(self.orchard), ironwood: f(self.ironwood) }
    }
}
