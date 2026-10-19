//! Who had a block on its best chain: a trusted validator, peers per netgroup (H7)

use zaino_primitives::types::{Netgroup, ECLIPSE_NETGROUPS};

/// Netgroups that make a peer quorum (more than [`ECLIPSE_NETGROUPS`])
const PEER_QUORUM: usize = ECLIPSE_NETGROUPS + 1;

/// - `Trusted` = a trusted validator (zebra commits only valid blocks)
/// - `Peer` = a live outbound peer's announcement, by its netgroup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Witness {
    Trusted,
    Peer(Netgroup),
}

/// - `netgroups` = the first [`PEER_QUORUM`] to witness it or a descendant (quorate for good)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Witnesses {
    vouched: bool,
    netgroups: [Option<Netgroup>; PEER_QUORUM],
}

impl Witnesses {
    pub(crate) fn vouched(&self) -> bool {
        self.vouched
    }

    pub(crate) fn peer_quorate(&self) -> bool {
        self.netgroups.iter().all(Option::is_some)
    }

    /// `false` = held already or quorate (a parent covers its child: the walk up stops)
    pub(crate) fn add(&mut self, by: Witness) -> bool {
        match by {
            Witness::Trusted => !std::mem::replace(&mut self.vouched, true),
            Witness::Peer(netgroup) if self.netgroups.contains(&Some(netgroup)) => false,
            Witness::Peer(netgroup) => {
                let free = self.netgroups.iter_mut().find(|slot| slot.is_none());
                free.map(|slot| *slot = Some(netgroup)).is_some()
            }
        }
    }

    /// H7: every witness of `child`, or quorate
    pub(crate) fn covers(&self, child: &Self) -> bool {
        let mut netgroups = child.netgroups.iter().flatten();
        let peers = self.peer_quorate() || netgroups.all(|n| self.netgroups.contains(&Some(*n)));
        (self.vouched || !child.vouched) && peers
    }
}
