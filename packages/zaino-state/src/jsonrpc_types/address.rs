use std::collections::HashSet;

use zebra_chain::transparent::Address;

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

    /// Returns the address strings as the client sent them.
    pub fn addresses(&self) -> &[String] {
        &self.addresses
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

/// Parses every address string, failing on the first that is not a valid transparent address.
pub fn valid_addresses(
    addresses: &[String],
) -> Result<HashSet<Address>, InvalidTransparentAddress> {
    addresses
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

/// A request for the transaction ids that touch a set of addresses, optionally within a height range.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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
    /// Creates a request for `addresses`, optionally bounded by `start` and `end`.
    pub fn new(addresses: Vec<String>, start: Option<u32>, end: Option<u32>) -> Self {
        Self {
            addresses,
            start,
            end,
        }
    }

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
