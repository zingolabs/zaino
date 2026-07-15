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
//! Following the hexagonal (ports/adapters) pattern, this crate depends on
//! nothing in `zaino-state`. Everything it needs from the outside world it
//! expresses as a *port* — a trait it defines itself (see [`ports`]). The outer
//! `zaino-state` crate supplies the *adapters* (concrete implementations of
//! [`ports::MempoolSource`] and [`ports::NfsEpochObserver`]) and injects them
//! into the mempool service. Dependencies always point inward: adapters know
//! about this core; this core never names a `zaino-state` type.
//!
//! Keeping the mempool behind these two small ports lets it live in its own
//! crate today, before the larger `zaino-state` decomposition (relocating
//! `BlockIndex`, `BlockchainSource`, and the non-finalized state into shared
//! crates) has happened.

pub mod error;
pub mod ports;

pub use error::MempoolError;
pub use ports::{BlockRef, MempoolSource, NfsEpochObserver, NonFinalizedEpoch};

/// A [`Future`](std::future::Future) that is [`Send`] and resolves to `T`.
///
/// Written as `impl SendFut<T>` in trait method return positions so the `Send`
/// bound is stated explicitly per method. This crate uses native
/// async-fn-in-trait rather than the `async-trait` macro, matching the
/// workspace convention (see `zaino-state`'s `SendFut` and the native-AFIT ADR).
pub trait SendFut<T>: std::future::Future<Output = T> + Send {}
impl<T, F: std::future::Future<Output = T> + Send> SendFut<T> for F {}
