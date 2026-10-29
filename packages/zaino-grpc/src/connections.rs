//! Accept-time caps (over a cap → socket closed, never queued)
//!
//! - Total = at accept, before a task exists
//! - Per client = once [`proxy_protocol`](crate::proxy_protocol) names it (behind a trusted
//!   proxy, only its PROXY header does), still before any HTTP/2 is spoken

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{metrics, GrpcLimits};

#[derive(Debug)]
pub(crate) struct ConnectionCaps {
    total: Arc<Semaphore>,
    total_max: usize,
    per_client: Arc<Mutex<HashMap<IpAddr, usize>>>,
    per_client_max: usize,
}

/// A connection's slot in the total cap, held while its client is identified
#[derive(Debug)]
pub(crate) struct Reserved {
    _slot: OwnedSemaphorePermit,
}

impl ConnectionCaps {
    pub(crate) fn new(limits: &GrpcLimits) -> Self {
        let total_max = limits.max_connections.get();
        Self {
            total: Arc::new(Semaphore::new(total_max)),
            total_max,
            per_client: Arc::new(Mutex::new(HashMap::new())),
            per_client_max: limits.max_connections_per_ip.get(),
        }
    }

    /// Connections held now (reserved or served)
    pub(crate) fn held(&self) -> usize {
        self.total_max.saturating_sub(self.total.available_permits())
    }

    /// `None` = at `max_connections` (caller drops the socket)
    pub(crate) fn reserve(&self) -> Option<Reserved> {
        match Arc::clone(&self.total).try_acquire_owned() {
            Ok(permit) => Some(Reserved { _slot: permit }),
            Err(_) => {
                metrics::connection_rejected();
                None
            }
        }
    }

    /// `None` = `client` at `max_connections_per_ip` (its reservation returns on drop)
    pub(crate) fn admit(&self, reserved: Reserved, client: IpAddr) -> Option<ConnectionGuard> {
        {
            let mut per_client = self.per_client.lock().expect("per-client table poisoned");
            let held = per_client.entry(client).or_insert(0);
            if *held >= self.per_client_max {
                metrics::connection_rejected();
                return None;
            }
            *held += 1;
        }

        metrics::connection_opened();
        Some(ConnectionGuard { _total: reserved, per_client: Arc::clone(&self.per_client), client })
    }
}

/// One connection's slot in both caps, held while served
#[derive(Debug)]
pub(crate) struct ConnectionGuard {
    _total: Reserved,
    per_client: Arc<Mutex<HashMap<IpAddr, usize>>>,
    client: IpAddr,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut per_client = self.per_client.lock().expect("per-client counts mutex poisoned");
        // Entry removed at zero (else an unbounded IP table = the cap's own leak)
        if let std::collections::hash_map::Entry::Occupied(mut held) = per_client.entry(self.client)
        {
            *held.get_mut() -= 1;
            if *held.get() == 0 {
                held.remove();
            }
        }
        metrics::connection_closed();
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    fn caps(max_connections: usize, max_connections_per_ip: usize) -> ConnectionCaps {
        let non_zero = |cap| NonZeroUsize::new(cap).expect("non-zero cap");
        ConnectionCaps::new(&GrpcLimits {
            max_connections: non_zero(max_connections),
            max_connections_per_ip: non_zero(max_connections_per_ip),
            ..GrpcLimits::default()
        })
    }

    #[test]
    fn connections_are_admitted_up_to_both_caps_and_every_slot_returns_on_close() {
        let caps = caps(3, 2);
        let (alice, bob): (IpAddr, IpAddr) = ([203, 0, 113, 1].into(), [203, 0, 113, 2].into());
        let admit = |client| caps.reserve().and_then(|reserved| caps.admit(reserved, client));

        let mut alices: Vec<_> = (0..3).map(|_| admit(alice)).collect();
        let bobs = admit(bob);
        let past_total = caps.reserve();

        let admitted: Vec<_> = alices.iter().map(Option::is_some).collect();
        assert_eq!(
            admitted,
            [true, true, false],
            "alice's 1st..=3rd under max_connections_per_ip 2"
        );
        assert!(bobs.is_some(), "bob counted apart from alice: {bobs:?}");
        assert_eq!(caps.held(), 3, "a refused client's reservation returned");
        assert!(past_total.is_none(), "4th connection over max_connections 3: {past_total:?}");
        alices.remove(0);
        assert!(admit(alice).is_some(), "one of alice's closed: her next admitted");

        drop((alices, bobs));
        let clients: Vec<_> =
            caps.per_client.lock().expect("per-client table").keys().copied().collect();
        assert_eq!(clients, Vec::<IpAddr>::new(), "every connection closed: no entry left");
        assert_eq!(caps.held(), 0, "every total slot returned");
    }
}
