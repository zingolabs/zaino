//! `NetworkType` spellings (two in use; never mix within one surface)

use zcash_protocol::consensus::NetworkType;

/// lightwalletd `chainName` / `z_gettreestate` `network` (BIP70; `regtest` = de-facto third)
pub fn chain_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    }
}

/// zainod config / log spelling
pub fn network_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "mainnet",
        NetworkType::Test => "testnet",
        NetworkType::Regtest => "regtest",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_network_has_one_chain_name_and_one_network_name() {
        let table = [
            (NetworkType::Main, "main", "mainnet"),
            (NetworkType::Test, "test", "testnet"),
            (NetworkType::Regtest, "regtest", "regtest"),
        ];
        for (network, chain, name) in table {
            assert_eq!((chain_name(network), network_name(network)), (chain, name), "{network:?}");
        }
    }
}
