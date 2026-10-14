//! [`MockPeerSet`]: the peer set as a [`PeerTransport`] over one [`MockChain`] path

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::FutureExt;
use zaino_primitives::testing::{encode_header, MockChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef};
use zaino_source::{FailureMode, NonDomainError};
use zcash_protocol::consensus::BlockHeight;

use crate::PeerTransport;

/// [`MockPeerSet`]s on a [`MockChain`]
pub trait PeerSetViews {
    /// Genesis ..= `tip`, all held
    fn peer_set(&self, tip: BlockRef) -> MockPeerSet;
}

impl PeerSetViews for MockChain {
    fn peer_set(&self, tip: BlockRef) -> MockPeerSet {
        let path = self.blocks(tip);
        let held = Mutex::new(path.len() - 1);
        MockPeerSet { path, held }
    }
}

/// Genesis ..= `held` of its path served: headers after the first locator hash held (zebra's
/// `FindHeaders`, ≤ 160), blocks by hash; a block unheld = `Connection` (as `ZebraPeers`)
pub struct MockPeerSet {
    path: Vec<Arc<Block>>,
    held: Mutex<usize>,
}

impl MockPeerSet {
    /// zebra `FindHeaders` answer cap
    const HEADERS: usize = 160;

    /// Its path held through `height` from now on (the network moving on, or not yet there)
    pub fn hold(&self, height: BlockHeight) {
        let at = u32::from(height) as usize;
        assert!(at < self.path.len(), "hold {height}: above the peer set's path");
        *self.held.lock().expect("MockPeerSet mutex poisoned") = at;
    }

    fn held(&self) -> &[Arc<Block>] {
        let held = *self.held.lock().expect("MockPeerSet mutex poisoned");
        &self.path[..=held]
    }
}

impl PeerTransport for MockPeerSet {
    fn headers(
        &self,
        locator: Vec<BlockHash>,
        _: Option<BlockHash>,
    ) -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>> {
        let held = self.held();
        let known = |hash: &BlockHash| held.iter().position(|block| block.header().hash == *hash);
        let after = locator.iter().find_map(known).map_or(&[][..], |at| &held[at + 1..]);
        let headers = after.iter().take(Self::HEADERS).map(|block| encode_header(block.header()));
        futures::future::ready(Ok(headers.collect())).boxed()
    }

    fn block(&self, hash: BlockHash) -> BoxFuture<'static, Result<Block, NonDomainError>> {
        let block = self.held().iter().find(|block| block.header().hash == hash);
        let absent = || NonDomainError::new(FailureMode::Connection, "MockPeerSet: no peer had it");
        futures::future::ready(block.map(|block| Block::clone(block)).ok_or_else(absent)).boxed()
    }
}
