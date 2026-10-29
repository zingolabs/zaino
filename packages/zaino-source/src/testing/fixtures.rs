//! Captured mainnet blocks (`tests/fixtures/block_<height>.hex`): bytes `MockChain` cannot mint

/// Every captured height
pub const HEIGHTS: [u32; 5] = [419_200, 1_000_000, 1_687_104, 2_000_000, 2_500_000];

/// Consensus bytes of the block at `height`
pub fn block(height: u32) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/block_{height}.hex", env!("CARGO_MANIFEST_DIR"));
    let hex = std::fs::read_to_string(&path).expect("fixture readable");
    const_hex::decode(hex.trim()).expect("fixture is hex")
}
