//! Holds tonic generated code for the lightwallet service RPCs and compact formats.

pub mod compact_formats;
pub mod proposal;
// The following mod is procedurally generated, with doc comments
// formatted in a way clippy doesn't like
#[allow(clippy::doc_overindented_list_items)]
pub mod service;
pub mod utils;
