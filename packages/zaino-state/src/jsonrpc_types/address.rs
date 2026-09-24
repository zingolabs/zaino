use std::collections::HashSet;

use derive_getters::Getters;
use derive_new::new;
use zebra_chain::{
    block::Height,
    transaction,
    transparent::{self, Address, OutputIndex},
};

/// A request for the transparent balance of a set of addresses.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(from = "DGetAddressBalanceRequest")]
pub struct GetAddressBalanceRequest {
    /// A list of transparent address strings.
    addresses: Vec<String>,
}

impl From<DGetAddressBalanceRequest> for GetAddressBalanceRequest {
    fn from(address_strings: DGetAddressBalanceRequest) -> Self {
        match address_strings {
            DGetAddressBalanceRequest::Addresses { addresses } => {
                GetAddressBalanceRequest { addresses }
            }
            DGetAddressBalanceRequest::Address(address) => GetAddressBalanceRequest {
                addresses: vec![address],
            },
        }
    }
}

/// The client forms a [`GetAddressBalanceRequest`] deserializes from.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Deserialize)]
#[serde(untagged)]
enum DGetAddressBalanceRequest {
    /// A list of address strings.
    Addresses { addresses: Vec<String> },
    /// A single address string.
    Address(String),
}

impl GetAddressBalanceRequest {
    /// Creates a request for the given address strings.
    pub fn new(addresses: Vec<String>) -> GetAddressBalanceRequest {
        GetAddressBalanceRequest { addresses }
    }
}

/// An address string that is not a valid transparent address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{address}: {reason}")]
pub struct InvalidTransparentAddress {
    /// The address as the client sent it.
    pub address: String,
    /// Why it did not parse.
    pub reason: String,
}

/// A request that carries address strings to be validated as transparent addresses.
pub trait ValidateAddresses {
    /// Parses every address string, failing on the first that is not a valid transparent address.
    fn valid_addresses(&self) -> Result<HashSet<Address>, InvalidTransparentAddress> {
        self.addresses()
            .iter()
            .map(|address| {
                address
                    .parse()
                    .map_err(|error: zebra_chain::serialization::SerializationError| {
                        InvalidTransparentAddress {
                            address: address.clone(),
                            reason: error.to_string(),
                        }
                    })
            })
            .collect()
    }

    /// Returns the address strings as the client sent them.
    fn addresses(&self) -> &[String];
}

impl ValidateAddresses for GetAddressBalanceRequest {
    fn addresses(&self) -> &[String] {
        &self.addresses
    }
}

/// A UTXO returned by the `getaddressutxos` request.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct GetAddressUtxos {
    /// The transparent address, base58check encoded.
    address: transparent::Address,

    /// The output txid, in big-endian order, hex-encoded.
    #[serde(with = "hex")]
    #[getter(copy)]
    txid: transaction::Hash,

    /// The transparent output index.
    #[serde(rename = "outputIndex")]
    #[getter(copy)]
    output_index: OutputIndex,

    /// The transparent output script, hex encoded.
    #[serde(with = "hex")]
    script: transparent::Script,

    /// The amount of zatoshis in the transparent output.
    satoshis: u64,

    /// The block height, last to match zcashd's field order.
    #[getter(copy)]
    height: Height,
}

impl GetAddressUtxos {
    /// Returns the UTXO's fields in declaration order.
    pub fn into_parts(
        &self,
    ) -> (
        transparent::Address,
        transaction::Hash,
        OutputIndex,
        transparent::Script,
        u64,
        Height,
    ) {
        (
            self.address,
            self.txid,
            self.output_index,
            self.script.clone(),
            self.satoshis,
            self.height,
        )
    }
}

/// A request for the transaction ids that touch a set of addresses, optionally within a height range.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, Getters, new)]
#[serde(from = "DGetAddressTxIdsRequest")]
pub struct GetAddressTxIdsRequest {
    /// The addresses whose transactions are requested.
    addresses: Vec<String>,
    /// The height to start looking for transactions.
    start: Option<u32>,
    /// The height to end looking for transactions.
    end: Option<u32>,
}

impl GetAddressTxIdsRequest {
    /// Returns the addresses and the range, with an absent bound as zero.
    pub fn into_parts(&self) -> (Vec<String>, u32, u32) {
        (
            self.addresses.clone(),
            self.start.unwrap_or(0),
            self.end.unwrap_or(0),
        )
    }
}

impl From<DGetAddressTxIdsRequest> for GetAddressTxIdsRequest {
    fn from(request: DGetAddressTxIdsRequest) -> Self {
        match request {
            DGetAddressTxIdsRequest::Single(addr) => GetAddressTxIdsRequest {
                addresses: vec![addr],
                start: None,
                end: None,
            },
            DGetAddressTxIdsRequest::Object {
                addresses,
                start,
                end,
            } => GetAddressTxIdsRequest {
                addresses,
                start,
                end,
            },
        }
    }
}

/// The client forms a [`GetAddressTxIdsRequest`] deserializes from.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum DGetAddressTxIdsRequest {
    /// A single address string.
    Single(String),
    /// A full request object with address list and optional height range.
    Object {
        /// A list of addresses to get transactions from.
        addresses: Vec<String>,
        /// The height to start looking for transactions.
        start: Option<u32>,
        /// The height to end looking for transactions.
        end: Option<u32>,
    },
}

impl ValidateAddresses for GetAddressTxIdsRequest {
    fn addresses(&self) -> &[String] {
        &self.addresses
    }
}
