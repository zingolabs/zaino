//! Helpers for the `clientless` partition of the live tests go here.
//!
//! This crate also exposes test-vectors.

/// Assert that `oracle` and `subject` return the same `getblockheader` response for
/// the block at `height`: look the block up on the oracle (verbosity 1) to learn its
/// hash, then compare the two servers' non-verbose header responses for that hash.
/// Shared body of the per-backend `get_block_header` oracle tests.
#[allow(deprecated)]
pub async fn assert_get_block_header_matches<Oracle, Subject>(
    oracle: &Oracle,
    subject: &Subject,
    height: u32,
) where
    Oracle: zaino_state::ZcashIndexer,
    Subject: zaino_state::ZcashIndexer,
{
    let block = oracle
        .z_get_block(height.to_string(), Some(1))
        .await
        .unwrap();

    let block_hash = match block {
        zebra_rpc::methods::GetBlock::Object(block) => block.hash(),
        zebra_rpc::methods::GetBlock::Raw(_) => panic!("Expected block object"),
    };

    let oracle_header = oracle
        .get_raw_block_header(block_hash.to_string())
        .await
        .unwrap();

    let subject_header = subject
        .get_raw_block_header(block_hash.to_string())
        .await
        .unwrap();
    assert_eq!(oracle_header, subject_header);
}

pub mod rpc {
    pub mod json_rpc {
        pub const VALID_P2PKH_ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
        pub const VALID_P2SH_ADDRESS: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";

        // Sprout vectors deliberately absent: Zaino does not classify Sprout
        // addresses, so there is nothing for a live test to assert beyond
        // "invalid", which `zaino-address`'s own tests already pin.

        pub const VALID_SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
        pub const VALID_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
        pub const VALID_DIVERSIFIED_TRANSMISSION_KEY: &str =
            "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";

        pub const VALID_UNIFIED_ADDRESS: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    }

    pub mod z_validate_address {
        use std::future::Future;

        use crate::rpc::json_rpc::{
            VALID_DIVERSIFIED_TRANSMISSION_KEY, VALID_DIVERSIFIER, VALID_P2PKH_ADDRESS,
            VALID_P2SH_ADDRESS, VALID_SAPLING_ADDRESS, VALID_UNIFIED_ADDRESS,
        };
        use zaino_address::ZValidatedAddress;
        #[allow(deprecated)]
        use zaino_state::ZcashIndexer;

        /// Decodes one of the hex test vectors into the fixed-size array the
        /// domain type carries. The vectors are hex because they were captured
        /// from zcashd's JSON output; the domain type is bytes because hex
        /// encoding is the wire layer's job.
        fn vector_bytes<const N: usize>(hex_vector: &str) -> [u8; N] {
            hex::decode(hex_vector)
                .expect("test vector is valid hex")
                .try_into()
                .expect("test vector has the expected length")
        }

        pub async fn run_z_validate_suite<F, Fut>(rpc_call: &F)
        where
            // Any callable that takes an address and returns the response (you can unwrap inside)
            F: Fn(String) -> Fut,
            Fut: Future<Output = ZValidatedAddress>,
        {
            assert_eq!(
                rpc_call(VALID_P2PKH_ADDRESS.to_string()).await,
                ZValidatedAddress::P2pkh {
                    address: VALID_P2PKH_ADDRESS.to_string()
                },
                "mismatch for P2PKH",
            );

            assert_eq!(
                rpc_call(VALID_P2SH_ADDRESS.to_string()).await,
                ZValidatedAddress::P2sh {
                    address: VALID_P2SH_ADDRESS.to_string()
                },
                "mismatch for P2SH",
            );

            // Sprout is not classified: `ZValidatedAddress` has no Sprout
            // variant, and a Sprout address is reported invalid. See that
            // type's Sprout note.

            assert_eq!(
                rpc_call(VALID_SAPLING_ADDRESS.to_string()).await,
                ZValidatedAddress::Sapling {
                    address: VALID_SAPLING_ADDRESS.to_string(),
                    diversifier: vector_bytes(VALID_DIVERSIFIER),
                    diversified_transmission_key: vector_bytes(VALID_DIVERSIFIED_TRANSMISSION_KEY),
                },
                "mismatch for Sapling",
            );

            // Unified (differs by validator)
            assert_eq!(
                rpc_call(VALID_UNIFIED_ADDRESS.to_string()).await,
                ZValidatedAddress::Unified {
                    address: VALID_UNIFIED_ADDRESS.to_string()
                },
                "mismatch for Unified",
            );

            // Invalids
            let by_len = rpc_call("t1123456789ABCDEFGHJKLMNPQRSTUVWXY".to_string()).await;
            let all_zeroes = rpc_call("t1000000000000000000000000000000000".to_string()).await;
            assert_eq!(by_len, ZValidatedAddress::Invalid);
            assert_eq!(all_zeroes, ZValidatedAddress::Invalid);
        }

        /// Build the `z_validate_address` rpc-call closure from `subscriber` and
        /// run the shared validation suite. Factors the identical closure +
        /// suite-call preamble shared by the four `z_validate_address` tests
        /// (fetch_service zcashd/zebrad, state_service, json_server).
        #[allow(deprecated)]
        pub async fn run_z_validate_for<S: ZcashIndexer>(subscriber: &S) {
            let rpc_call =
                |addr: String| async move { subscriber.z_validate_address(addr).await.unwrap() };
            run_z_validate_suite(&rpc_call).await;
        }
    }
}
