//! `zaino-mempool` — Zaino's bounded, coherent local mempool read-model.
//!
//! This crate is the hexagonal *core* of the mempool subsystem. It owns the
//! domain logic for maintaining a fast, bounded, coherent view of the
//! validator's unconfirmed transactions and serving it to Zaino's ChainIndex and
//! RPC layers. It is **not** a validator mempool: it does not validate
//! transactions, implement fee policy, gossip, or reproduce eviction logic.
//!
//! # Ports and adapters
//!
//! Following the hexagonal (ports/adapters) pattern, this crate holds the domain
//! types and the ports, and depends on nothing in `zaino-state`. It reads the
//! validator through [`zaino-source`](zaino_source)'s ports, naming the subset it
//! needs as [`ports::MempoolSource`]; the two things `zaino-source` cannot
//! describe — Zaino's own non-finalized-state epoch, and the read models this
//! crate offers — are ports declared here.
//!
//! Concrete *adapters* live one layer out in
//! [`zaino-mempool-service`](https://docs.rs/zaino-mempool-service) (the polling
//! `MempoolService`, the `MempoolSubscriber` read handle, and the coherence
//! layer), and `zaino-state` supplies the
//! [`ports::NfsEpochObserver`] implementation over its
//! non-finalized state. Dependencies always point inward: adapters know about
//! this core; this core never names an adapter or a `zaino-state` type.
//!
//! It also names no node library. Entries hold the validator's bytes as
//! [`Bytes`](bytes::Bytes) and never parse them, so nothing here depends on
//! `zebra-chain` — the parse belongs to whichever layer needs a transaction.

pub mod config;
pub mod entry;
pub mod error;
pub mod ports;
pub mod snapshot;
pub mod update;

#[cfg(feature = "tip_aware_mempool")]
pub mod event;
#[cfg(feature = "tip_aware_mempool")]
pub mod tip;

pub use config::MempoolConfig;
pub use entry::MempoolEntry;
pub use error::MempoolError;
pub use ports::{Mempool, MempoolSource};
pub use snapshot::{reversed_txid_key, MempoolCompleteness, MempoolSnapshot};
pub use update::MempoolUpdate;

#[cfg(feature = "tip_aware_mempool")]
pub use event::MempoolEvent;
#[cfg(feature = "tip_aware_mempool")]
pub use ports::{MempoolStreamError, NfsEpochObserver, NoNfs, NonFinalizedEpoch, TipAwareMempool};
#[cfg(feature = "tip_aware_mempool")]
pub use tip::{CoherentSnapshot, FreezeReason, MempoolMode, ObservedTips, TipChange};
