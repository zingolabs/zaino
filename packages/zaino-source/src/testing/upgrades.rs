//! Regtest schedules for a [`MockChain`](super::MockChain) (zebrad regtest's config)

use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade};
use zcash_protocol::local_consensus::LocalNetwork;

/// Overwinter ..= NU6.3 at `height`, NU7 off
pub fn all_at(height: BlockHeight) -> LocalNetwork {
    LocalNetwork::from_activation_heights(|upgrade| {
        (upgrade != NetworkUpgrade::Nu7).then_some(height)
    })
}
