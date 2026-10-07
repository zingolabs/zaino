//! Accept-time caps (over a cap → socket closed, never queued)
//!
//! - Total = at accept, before a task exists
//! - Per client = once [`client`](crate::client) names it (behind a trusted proxy, only its PROXY
//!   header does), still before any HTTP/2 is spoken

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{emit, GrpcLimits};

#[derive(Clone, Debug)]
pub(crate) struct ConnectionCaps {
    total: Arc<Semaphore>,
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
        Self {
            total: Arc::new(Semaphore::new(limits.max_connections.get())),
            per_client: Arc::new(Mutex::new(HashMap::new())),
            per_client_max: limits.max_connections_per_ip.get(),
        }
    }

    /// Connections held now (reserved or served), of `cap` (`max_connections`)
    pub(crate) fn held(&self, cap: usize) -> (usize, usize) {
        (cap.saturating_sub(self.total.available_permits()), cap)
    }

    /// `None` = at `max_connections` (caller drops the socket)
    pub(crate) fn reserve(&self) -> Option<Reserved> {
        match Arc::clone(&self.total).try_acquire_owned() {
            Ok(permit) => Some(Reserved { _slot: permit }),
            Err(_) => {
                emit::connection_rejected();
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
                emit::connection_rejected();
                return None;
            }
            *held += 1;
        }

        emit::connection_opened();
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
        if let Ok(mut per_client) = self.per_client.lock() {
            // Entry removed at zero (else an unbounded IP table = the cap's own leak)
            if let std::collections::hash_map::Entry::Occupied(mut held) =
                per_client.entry(self.client)
            {
                *held.get_mut() -= 1;
                if *held.get() == 0 {
                    held.remove();
                }
            }
        }
        emit::connection_closed();
    }
}
