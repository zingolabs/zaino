// TODO: Decide if having a "proofs" feature flag is a good idea.

pub mod v1 {
    pub mod tags {
        pub const STREAMING: &[u8] = b"ZAINO-UTXOSET-V1\0";
        pub const SMT: &[u8] = b"ZAINO-UTXOSET-V1-SMT\0";
        pub const KEY: &[u8] = b"ZAINO-KEY-V1\0";
        // TODO: LEAF / NODE / EMPTY domain tags...
    }

    /// CompactSize encoder for scriptPubKey lengths.
    pub fn compact_size_len(n: usize) -> Vec<u8> {
        todo!()
    }

    /// Canonical key derivation: BLAKE3(KEY_TAG || txid || le_u32(index))
    pub fn utxo_key(txid32: [u8; 32], vout: u32) -> [u8; 32] {
        todo!()
    }

    /// Leaf commitment over value_zat and scriptPubKey, with domain tag.
    pub fn leaf_commitment(value_zat: i64, script: &[u8]) -> [u8; 32] {
        todo!()
    }

    /// Streaming digest
    pub struct StreamingDigest {}
    impl StreamingDigest {
        pub fn new() -> Self {
            todo!()
        }
        pub fn update(&mut self, txid: [u8; 32], vout: u32, value_zat: i64, script: &[u8]) {
            todo!()
        }
        pub fn finalize(self) -> [u8; 32] {
            todo!()
        }
    }

    /// SMT builder/updater
    pub struct Smt {
        pub count_txouts: u64,
        pub root: [u8; 32],
        // TODO: Store a sparse map or a callback interface
    }
    impl Smt {
        pub fn new_empty() -> Self {
            todo!()
        }

        /// MSB-first, 256 levels
        pub fn upsert(&mut self, key: [u8; 32], leaf: [u8; 32]) {
            todo!()
        }
        pub fn delete(&mut self, key: [u8; 32]) {
            todo!()
        }

        /// BLAKE3(header||count||root)
        pub fn snapshot_digest(&self, header: &[u8; 32]) -> [u8; 32] {
            todo!()
        }

        #[cfg(feature = "proofs")]
        pub fn prove(&self, key: [u8; 32]) -> Proof {
            todo!()
        }
    }

    #[cfg(feature = "proofs")]
    pub struct Proof {
        pub siblings: Vec<[u8; 32]>,
        pub leaf: [u8; 32],
    }
    #[cfg(feature = "proofs")]
    impl Proof {
        pub fn verify(&self, key: [u8; 32], expected_root: [u8; 32]) -> bool {
            todo!()
        }
    }
}
