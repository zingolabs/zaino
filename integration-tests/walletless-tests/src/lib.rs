//! Helpers for integration-tests go here.
//!
//! This crate also exposes test-vectors.

pub mod rpc {
    pub mod json_rpc {
        pub const VALID_P2PKH_ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
        pub const VALID_P2SH_ADDRESS: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";

        pub const VALID_SPROUT_ADDRESS: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";
        pub const VALID_PAYING_KEY: &str =
            "c8e8797f1fb5e9cf6b2d000177c5994119279a2629970a4f669aed1362a4cca5";
        pub const VALID_TRANSMISSION_KEY: &str =
            "480f78d61bdd7fc4b4edeef9f6305b29753057ab1008d42ded1a3364dac2d83c";

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
        use zaino_fetch::jsonrpsee::response::z_validate_address::{
            KnownZValidateAddress, ValidZValidateAddress, ZValidateAddressResponse,
        };
        #[allow(deprecated)]
        use zaino_state::ZcashIndexer;

        pub fn assert_known_valid_eq(
            resp: ZValidateAddressResponse,
            expected: ValidZValidateAddress,
            label: &str,
        ) {
            match resp {
                ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(actual)) => {
                    assert_eq!(actual, expected, "mismatch for {label}")
                }
                other => panic!(
                    "Unexpected ZValidateAddressResponse for {label}: {:#?}",
                    other
                ),
            }
        }

        pub async fn run_z_validate_suite<F, Fut>(rpc_call: &F)
        where
            // Any callable that takes an address and returns the response (you can unwrap inside)
            F: Fn(String) -> Fut,
            Fut: Future<Output = ZValidateAddressResponse>,
        {
            // P2PKH
            let expected_p2pkh = ValidZValidateAddress::p2pkh(VALID_P2PKH_ADDRESS.to_string());
            assert_known_valid_eq(
                rpc_call(VALID_P2PKH_ADDRESS.to_string()).await,
                expected_p2pkh,
                "P2PKH",
            );

            // P2SH
            let expected_p2sh = ValidZValidateAddress::p2sh(VALID_P2SH_ADDRESS.to_string());
            assert_known_valid_eq(
                rpc_call(VALID_P2SH_ADDRESS.to_string()).await,
                expected_p2sh,
                "P2SH",
            );

            // Note: It could be the case that Zaino needs to support Sprout. For now, it's been disabled.

            // let expected_sprout = ZValidateAddress::sprout("ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ".to_string(), "c8e8797f1fb5e9cf6b2d000177c5994119279a2629970a4f669aed1362a4cca5".to_string(), "480f78d61bdd7fc4b4edeef9f6305b29753057ab1008d42ded1a3364dac2d83c".to_string());

            // let fs_sprout = zcashd_subscriber
            //     .z_validate_address("ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ".to_string())
            //     .await
            //     .unwrap();

            // assert_eq!(fs_sprout, expected_sprout);

            // Sapling (differs by validator)

            // Unified (differs by validator)
            let expected_unified =
                ValidZValidateAddress::unified(VALID_UNIFIED_ADDRESS.to_string());
            assert_known_valid_eq(
                rpc_call(VALID_UNIFIED_ADDRESS.to_string()).await,
                expected_unified,
                "Unified",
            );

            // Invalids
            let by_len = rpc_call("t1123456789ABCDEFGHJKLMNPQRSTUVWXY".to_string()).await;
            let all_zeroes = rpc_call("t1000000000000000000000000000000000".to_string()).await;
            assert_eq!(by_len, ZValidateAddressResponse::invalid());
            assert_eq!(all_zeroes, ZValidateAddressResponse::invalid());
        }

        pub async fn run_z_validate_sapling<F, Fut>(rpc_call: &F)
        where
            F: Fn(String) -> Fut,
            Fut: Future<Output = ZValidateAddressResponse>,
        {
            let expected_sapling = ValidZValidateAddress::sapling(
                VALID_SAPLING_ADDRESS.to_string(),
                Some(VALID_DIVERSIFIER.to_string()),
                Some(VALID_DIVERSIFIED_TRANSMISSION_KEY.to_string()),
            );
            assert_known_valid_eq(
                rpc_call(VALID_SAPLING_ADDRESS.to_string()).await,
                expected_sapling,
                "Sapling",
            );
        }

        /// zebrad's JSON-RPC passthrough (via FetchService) omits `diversifier`
        /// and `diversifiedtransmissionkey` from the Sapling response. This is
        /// the safer behavior: address component extraction should happen
        /// client-side, not by delegating to a remote actor.
        ///
        /// See [`DEPRECATION_NOTICE`](zaino_fetch::jsonrpsee::response::z_validate_address::DEPRECATION_NOTICE).
        pub async fn run_z_validate_sapling_zebrad_passthrough_fetchservice<F, Fut>(rpc_call: &F)
        where
            F: Fn(String) -> Fut,
            Fut: Future<Output = ZValidateAddressResponse>,
        {
            let expected_sapling = ValidZValidateAddress::sapling(
                VALID_SAPLING_ADDRESS.to_string(),
                None::<String>,
                None::<String>,
            );
            assert_known_valid_eq(
                rpc_call(VALID_SAPLING_ADDRESS.to_string()).await,
                expected_sapling,
                "Sapling (zebrad passthrough via FetchService — keys omitted)",
            );
        }

        /// Which sapling suite to run after the shared suite.
        pub enum SaplingSuite {
            /// Full response — diversifier and diversifiedtransmissionkey present.
            Standard,
            /// zebrad's JSON-RPC passthrough (via FetchService) omits those keys.
            ZebradPassthroughFetchService,
        }

        /// Build the `z_validate_address` rpc-call closure from `subscriber` and
        /// run the shared validation suite plus the chosen sapling suite. Factors
        /// the identical closure + suite-call preamble shared by the four
        /// `z_validate_address` tests (fetch_service zcashd/zebrad, state_service,
        /// json_server).
        #[allow(deprecated)]
        pub async fn run_z_validate_for<S: ZcashIndexer>(subscriber: &S, sapling: SaplingSuite) {
            let rpc_call =
                |addr: String| async move { subscriber.z_validate_address(addr).await.unwrap() };
            run_z_validate_suite(&rpc_call).await;
            match sapling {
                SaplingSuite::Standard => run_z_validate_sapling(&rpc_call).await,
                SaplingSuite::ZebradPassthroughFetchService => {
                    run_z_validate_sapling_zebrad_passthrough_fetchservice(&rpc_call).await
                }
            }
        }
    }
}
