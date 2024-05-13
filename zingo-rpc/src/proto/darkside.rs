//! Generated Proto Code
//!
//! Remember that proto3 fields are all optional. A field that is not present will be set to its zero value.
//! bytes fields of hashes are in canonical little-endian format.

use zcash_client_backend::proto::service::ShieldedProtocol;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideMetaState {
    #[prost(int32, tag = "1")]
    pub sapling_activation: i32,
    #[prost(string, tag = "2")]
    pub branch_id: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub chain_name: ::prost::alloc::string::String,
    #[prost(uint32, tag = "4")]
    pub start_sapling_commitment_tree_size: u32,
    #[prost(uint32, tag = "5")]
    pub start_orchard_commitment_tree_size: u32,
}
/// A block is a hex-encoded string.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideBlock {
    #[prost(string, tag = "1")]
    pub block: ::prost::alloc::string::String,
}
/// DarksideBlocksURL is typically something like:
/// <https://raw.githubusercontent.com/zcash-hackworks/darksidewalletd-test-data/master/basic-reorg/before-reorg.txt>
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideBlocksUrl {
    #[prost(string, tag = "1")]
    pub url: ::prost::alloc::string::String,
}
/// DarksideTransactionsURL refers to an HTTP source that contains a list
/// of hex-encoded transactions, one per line, that are to be associated
/// with the given height (fake-mined into the block at that height)
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideTransactionsUrl {
    #[prost(int32, tag = "1")]
    pub height: i32,
    #[prost(string, tag = "2")]
    pub url: ::prost::alloc::string::String,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideHeight {
    #[prost(int32, tag = "1")]
    pub height: i32,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideEmptyBlocks {
    #[prost(int32, tag = "1")]
    pub height: i32,
    #[prost(int32, tag = "2")]
    pub nonce: i32,
    #[prost(int32, tag = "3")]
    pub count: i32,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideSubtreeRoots {
    #[prost(enumeration = "ShieldedProtocol", tag = "1")]
    pub shielded_protocol: i32,
    #[prost(uint32, tag = "2")]
    pub start_index: u32,
    #[prost(message, repeated, tag = "3")]
    pub subtree_roots: ::prost::alloc::vec::Vec<zcash_client_backend::proto::service::SubtreeRoot>,
}
