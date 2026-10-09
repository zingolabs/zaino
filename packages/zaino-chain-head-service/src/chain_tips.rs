//! Chain tips over the non-finalised window.
//!
//! The window is the only tier that holds more than one branch: the finalised
//! store is a single chain by construction. So the retained graph answers
//! `getchaintips` whole, with no finalised half to compose.

use zaino_chain_head::ChainHeadSnapshot;
use zaino_primitives::types::rpc::ChainTip;
use zaino_service::ChainTipsRead;

use crate::serve::HeadSnapshot;

impl ChainTipsRead for HeadSnapshot {
    fn chain_tips(&self) -> Vec<ChainTip> {
        self.window().chain_tips()
    }
}
