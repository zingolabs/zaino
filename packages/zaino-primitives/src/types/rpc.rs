//! Response shapes for JSON-RPC methods Zaino proxies to its backing validator.
//!
//! These types sit apart from the rest of [`super`] because they are a
//! different kind of thing. The types in the parent module are Zaino's model of
//! the chain — Zaino builds them, indexes them, and reasons about them. The
//! types here are *answers Zaino forwards*: it asks the validator a question,
//! translates the reply into one of these, and passes it on. Zaino has no
//! opinion about their contents.
//!
//! # Contract
//!
//! The RPC source backend is generic: it must work against any validator that
//! serves the Zcash JSON-RPC interface, not just Zebra. That imposes two rules,
//! which are deliberately separate mechanisms:
//!
//! 1. **Parsing is lenient, and that lives in the adapter.** Wire types carry
//!    `#[serde(default)]` and never `deny_unknown_fields`, so a validator that
//!    omits a field or adds one Zaino has never seen still parses. A field the
//!    validator did not send arrives here as `None`.
//! 2. **These types carry only named, typed, usable fields.** Fields Zaino does
//!    not model are dropped at the adapter boundary rather than being carried
//!    through as opaque JSON. A value Zaino cannot type is a value Zaino cannot
//!    validate or reason about, and it has no place in a business type.
//!
//! Leniency buys compatibility; it does not require passthrough. Those were
//! conflated in the previous `zaino-fetch` wire types, which were inconsistent
//! about it: one type forwarded unknown fields via `#[serde(flatten)]` while
//! four others set `deny_unknown_fields` and *failed* on them — the latter
//! being outright hostile to a generic backend.
//!
//! # Optionality
//!
//! `Option` here means "this validator may not report it", never "we could not
//! be bothered to parse it". Each optional field documents which validators
//! supply it and why it is worth keeping. Fields that only ever came from
//! zcashd, which is being deprecated, are modelled only where they carry
//! information a consumer can actually use.

mod address_deltas;
mod block_deltas;
mod block_header;
mod block_subsidy;
mod chain_tip;
mod mining_info;
mod node_info;
mod peer_info;
mod spent_info;
mod subtree_roots;
mod tx_out;

pub use address_deltas::{AddressDeltas, AddressDeltasRequest};
pub use block_deltas::{BlockDelta, BlockDeltas, InputDelta, OutputDelta};
pub use block_header::BlockHeaderVerbose;
pub use block_subsidy::{BlockSubsidy, FundingStream, LockboxStream};
pub use chain_tip::{ChainTip, ChainTipStatus};
pub use mining_info::MiningInfo;
pub use node_info::NodeInfo;
pub use peer_info::PeerInfo;
pub use spent_info::{SpentInfo, SpentOutpoint};
pub use subtree_roots::SubtreeRoots;
pub use tx_out::{ScriptPubKey, TxOut};
