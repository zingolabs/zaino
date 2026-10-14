//! lightwalletd's `NetworkType` spelling (zainod's config spelling = `network_name` there)

use zcash_protocol::consensus::NetworkType;

/// lightwalletd `chainName` / `z_gettreestate` `network` (BIP70; `regtest` = de-facto third)
pub(crate) fn chain_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_network_has_one_chain_name() {
        let table = [
            (NetworkType::Main, "main"),
            (NetworkType::Test, "test"),
            (NetworkType::Regtest, "regtest"),
        ];
        for (network, chain) in table {
            assert_eq!(chain_name(network), chain, "{network:?}");
        }
    }
}
