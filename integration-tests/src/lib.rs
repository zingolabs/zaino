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
}
