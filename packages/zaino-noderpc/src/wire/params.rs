//! Request parameters, as the explorer's client sends them.
//!
//! Shapes follow `nighthawk-apps/zcashex`, the client real callers use, rather
//! than the prose in the zcashd RPC docs: the address RPCs take a single object
//! parameter, not a positional list.

use serde::Deserialize;

/// The `{"addresses": [...]}` object the address RPCs take as their one
/// positional parameter.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressesParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
}
