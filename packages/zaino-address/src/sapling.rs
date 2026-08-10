//! Sapling payment-address component extraction.

/// Extracts the diversifier and pk_d bytes from a validated Sapling
/// [`sapling_crypto::PaymentAddress`], returning pk_d in zcashd's big-endian
/// byte order.
///
/// # Deprecation
///
/// See [`DEPRECATION_NOTICE`](crate::DEPRECATION_NOTICE). This function exists
/// to support the `z_validateaddress` RPC, which itself exists solely for
/// zcashd compatibility. The pk_d bytes are reversed from `sapling-crypto`'s
/// native little-endian representation to match zcashd's big-endian hex output.
///
/// # Precondition
///
/// The caller must have obtained `address` through `PaymentAddress::from_bytes`
/// or equivalent (e.g. `ZcashAddress::convert_if_network`), which guarantees
/// the diversifier has a valid `g_d()` and pk_d is a non-identity Jubjub
/// subgroup point. No additional validation is performed here.
///
/// # Layout
///
/// `PaymentAddress::to_bytes()` returns 43 bytes: `diversifier (11) || pk_d (32)`.
pub fn sapling_key_bytes(address: &sapling_crypto::PaymentAddress) -> ([u8; 11], [u8; 32]) {
    let bytes = address.to_bytes();
    let diversifier: [u8; 11] = bytes[..11]
        .try_into()
        .expect("PaymentAddress::to_bytes always returns 43 bytes: diversifier is the first 11");
    let mut pk_d: [u8; 32] = bytes[11..]
        .try_into()
        .expect("PaymentAddress::to_bytes always returns 43 bytes: pk_d is the last 32");
    pk_d.reverse();
    (diversifier, pk_d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classifies the byte-level relationship between two slices.
    #[derive(Debug, PartialEq)]
    enum ByteRelation {
        /// The slices are identical.
        Equal,
        /// `actual` fully byte-reversed equals `expected` (endian swap).
        FullByteReversal,
        /// Each byte's bits reversed maps `actual` to `expected`.
        PerByteBitReversal,
        /// Reversing bytes within 16-bit chunks maps `actual` to `expected`.
        ChunkSwap16,
        /// Reversing bytes within 32-bit chunks maps `actual` to `expected`.
        ChunkSwap32,
        /// Reversing bytes within 64-bit chunks maps `actual` to `expected`.
        ChunkSwap64,
        /// No recognized transformation.
        Unrecognized,
    }

    impl std::fmt::Display for ByteRelation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Equal => write!(f, "equal"),
                Self::FullByteReversal => write!(f, "full byte-reversal (endian swap)"),
                Self::PerByteBitReversal => write!(f, "per-byte bit-reversal"),
                Self::ChunkSwap16 => write!(f, "16-bit pairwise byte-swap"),
                Self::ChunkSwap32 => write!(f, "32-bit chunk byte-reversal"),
                Self::ChunkSwap64 => write!(f, "64-bit chunk byte-reversal"),
                Self::Unrecognized => write!(f, "unrecognized mismatch"),
            }
        }
    }

    /// Applies each candidate byte transformation to `actual` and returns
    /// the first that produces `expected`, or [`ByteRelation::Unrecognized`].
    // `u32::is_multiple_of` is only stable from Rust 1.87; keep `% n == 0` for our older MSRV.
    #[allow(clippy::manual_is_multiple_of)]
    fn classify_byte_relation(actual: &[u8], expected: &[u8]) -> ByteRelation {
        if actual == expected {
            return ByteRelation::Equal;
        }

        let chunk_swap = |size: usize| -> Vec<u8> {
            actual
                .chunks(size)
                .flat_map(|c| c.iter().rev())
                .copied()
                .collect()
        };

        let mut reversed = actual.to_vec();
        reversed.reverse();
        if reversed == expected {
            return ByteRelation::FullByteReversal;
        }

        let bit_reversed: Vec<u8> = actual.iter().map(|b| b.reverse_bits()).collect();
        if bit_reversed == expected {
            return ByteRelation::PerByteBitReversal;
        }

        if actual.len() % 2 == 0 && chunk_swap(2) == expected {
            return ByteRelation::ChunkSwap16;
        }
        if actual.len() % 4 == 0 && chunk_swap(4) == expected {
            return ByteRelation::ChunkSwap32;
        }
        if actual.len() % 8 == 0 && chunk_swap(8) == expected {
            return ByteRelation::ChunkSwap64;
        }

        ByteRelation::Unrecognized
    }

    /// Verifies that our Sapling address parsing logic produces the same
    /// diversifier and diversified transmission key (pk_d) bytes as zcashd's
    /// `z_validateaddress` RPC.
    ///
    /// # Guarantees
    ///
    /// - Exercises the production [`sapling_key_bytes`] function directly.
    /// - The 11-byte diversifier matches the zcashd-derived test vector.
    /// - The 32-byte pk_d (after the endian reversal inside
    ///   [`sapling_key_bytes`]) matches the zcashd-derived test vector.
    /// - If the upstream serialization changes, the failure message classifies
    ///   the mismatch (endian swap, bit-reversal, chunk swap, or unrecognized)
    ///   to aid diagnosis.
    ///
    /// # Non-guarantees
    ///
    /// - Does not prove the test vector constants themselves are correct; they
    ///   were captured from zcashd and are trusted as ground truth.
    /// - Does not verify behavior for malformed Sapling addresses or addresses
    ///   on other networks (mainnet, testnet).
    #[test]
    fn sapling_pk_d_byte_order_matches_test_vector() {
        use zcash_keys::address::Address;
        use zcash_protocol::consensus::NetworkType;

        // Canonical source: live-tests/clientless/src/lib.rs::rpc::json_rpc
        // Tracked for DRY consolidation: https://github.com/zingolabs/zaino/issues/988
        const SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
        const EXPECTED_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
        const EXPECTED_PK_D: &str =
            "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";

        let parsed: zcash_address::ZcashAddress = SAPLING_ADDRESS.parse().unwrap();
        let converted = parsed
            .convert_if_network::<Address>(NetworkType::Regtest)
            .unwrap();

        let Address::Sapling(sapling) = converted else {
            panic!("expected Sapling address");
        };

        let (diversifier, pk_d) = sapling_key_bytes(&sapling);

        let expected_diversifier = hex::decode(EXPECTED_DIVERSIFIER).unwrap();
        let expected_pk_d = hex::decode(EXPECTED_PK_D).unwrap();

        match classify_byte_relation(&diversifier, &expected_diversifier) {
            ByteRelation::Equal => {}
            relation => panic!(
                "diversifier mismatch.\n  relation: {relation}\n  actual:   {}\n  expected: {}",
                hex::encode(diversifier),
                hex::encode(expected_diversifier),
            ),
        }

        // pk_d (sapling_key_bytes already applies the endian reversal)
        match classify_byte_relation(&pk_d, &expected_pk_d) {
            ByteRelation::Equal => {}
            relation => panic!(
                "pk_d mismatch — upstream serialization may have changed.\
                \n  relation: {relation}\n  actual:   {}\n  expected: {}",
                hex::encode(pk_d),
                hex::encode(expected_pk_d),
            ),
        }
    }

    #[test]
    fn classify_byte_relation_detects_known_transforms() {
        let original = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

        assert_eq!(
            classify_byte_relation(&original, &original),
            ByteRelation::Equal,
        );

        let mut reversed = original.to_vec();
        reversed.reverse();
        assert_eq!(
            classify_byte_relation(&original, &reversed),
            ByteRelation::FullByteReversal,
        );

        let bit_rev: Vec<u8> = original.iter().map(|b| b.reverse_bits()).collect();
        assert_eq!(
            classify_byte_relation(&original, &bit_rev),
            ByteRelation::PerByteBitReversal,
        );

        let swapped_16: Vec<u8> = original
            .chunks(2)
            .flat_map(|c| c.iter().rev())
            .copied()
            .collect();
        assert_eq!(
            classify_byte_relation(&original, &swapped_16),
            ByteRelation::ChunkSwap16,
        );

        let garbage = [0xFF; 8];
        assert_eq!(
            classify_byte_relation(&original, &garbage),
            ByteRelation::Unrecognized,
        );
    }
}
