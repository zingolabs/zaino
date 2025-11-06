//! Types associated with the `getinfo` RPC request.

use std::convert::Infallible;

use crate::jsonrpsee::{connector::ResponseToError, response::ErrorsTimestamp};

/// Response to a `getinfo` RPC request.
///
/// This is used for the output parameter of [`crate::jsonrpsee::connector::JsonRpSeeConnector::get_info`].
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct GetInfoResponse {
    /// The node version
    #[serde(default)]
    version: u64,
    /// The node version build number
    pub build: String,
    /// The server sub-version identifier, used as the network protocol user-agent
    pub subversion: String,
    /// The protocol version
    #[serde(default)]
    #[serde(rename = "protocolversion")]
    protocol_version: u32,

    /// The current number of blocks processed in the server
    #[serde(default)]
    blocks: u32,

    /// The total (inbound and outbound) number of connections the node has
    #[serde(default)]
    connections: usize,

    /// The proxy (if any) used by the server. Currently always `None` in Zebra.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,

    /// The current network difficulty
    #[serde(default)]
    difficulty: f64,

    /// True if the server is running in testnet mode, false otherwise
    #[serde(default)]
    testnet: bool,

    /// The minimum transaction fee in ZEC/kB
    #[serde(default)]
    #[serde(rename = "paytxfee")]
    pay_tx_fee: f64,

    /// The minimum relay fee for non-free transactions in ZEC/kB
    #[serde(default)]
    #[serde(rename = "relayfee")]
    relay_fee: f64,

    /// The last error or warning message, or "no errors" if there are no errors
    #[serde(default)]
    errors: String,

    /// The time of the last error or warning message, or "no errors timestamp" if there are no errors
    #[serde(default)]
    #[serde(rename = "errorstimestamp")]
    errors_timestamp: ErrorsTimestamp,
}

impl ResponseToError for GetInfoResponse {
    type RpcError = Infallible;
}

impl From<GetInfoResponse> for zebra_rpc::methods::GetInfo {
    fn from(response: GetInfoResponse) -> Self {
        zebra_rpc::methods::GetInfo::new(
            response.version,
            response.build,
            response.subversion,
            response.protocol_version,
            response.blocks,
            response.connections,
            response.proxy,
            response.difficulty,
            response.testnet,
            response.pay_tx_fee,
            response.relay_fee,
            response.errors,
            response.errors_timestamp.to_string(),
        )
    }
}
