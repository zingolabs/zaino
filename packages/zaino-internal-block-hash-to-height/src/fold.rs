//! Block → its one `by_hash` row (pure, no parent read: a row depends on its own header only)

use zaino_persistence::Changes;
use zaino_primitives::types::{Block, BlockRef};
use zcash_protocol::consensus::NetworkType;

use crate::{
    by_hash::{encode_height, BY_HASH},
    schema, HASH,
};

pub fn fold(block: &Block, network: NetworkType) -> Changes {
    let header = block.header();
    let at = BlockRef { hash: header.hash, height: header.height };
    let mut changes = Changes::new(at, &schema(network));
    changes.insert(BY_HASH, &<[u8; HASH]>::from(header.hash), &encode_height(header.height));
    changes
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store};
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{BlockHash, Height};

    use super::*;
    use crate::BlockHashReader;

    /// Each block → one golden row at its own tip; folded over an in-memory view, the reader
    /// locates every folded hash and nothing else
    #[test]
    fn each_block_folds_to_its_golden_row_and_the_reader_locates_exactly_those() {
        let network = NetworkType::Regtest;
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 2);
        let blocks = chain.path(tip.hash);
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/bh"), &schema(network));
        let mut store = store.expect("open");

        for (height, block) in (0u8..).zip(&blocks) {
            let header = block.header();
            let hash = <[u8; HASH]>::from(header.hash);
            let changes = fold(block, network);
            let rows: Vec<(&[u8], &[u8])> = changes.inserts(BY_HASH).collect();
            assert_eq!(rows, [(&hash[..], &[0, 0, 0, height][..])], "block {height}: hash → BE");
            assert_eq!(changes.tip(), BlockRef { hash: header.hash, height: header.height });
            store.apply(changes);
        }

        let reader = BlockHashReader::new(store.staged());
        let located: Vec<_> =
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect();
        let expected: Vec<_> = (0..3u32).map(|n| Some(Height::try_from(n).expect("h"))).collect();
        assert_eq!(located, expected);
        assert_eq!(reader.height_of(&BlockHash::from([0xee; HASH])), None, "never folded");
    }
}
