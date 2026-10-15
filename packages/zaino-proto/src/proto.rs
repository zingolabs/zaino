//! Tonic-generated lightwallet service RPCs + compact formats, zebrad's indexer service

#[allow(clippy::all)]
pub mod compact_formats {
    include!(concat!(env!("OUT_DIR"), "/compact_formats/cash.z.wallet.sdk.rpc.rs"));
}
#[allow(clippy::all)]
pub mod service {
    include!(concat!(env!("OUT_DIR"), "/service/cash.z.wallet.sdk.rpc.rs"));
}
#[allow(clippy::all)]
pub mod zebra_indexer {
    include!(concat!(env!("OUT_DIR"), "/zebra_indexer/zebra.indexer.rpc.rs"));
}
