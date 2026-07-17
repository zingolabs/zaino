//! `zaino-mempool-rpc` — concrete adapters/implementations of the mempool ports.
//!
//! This crate is the hexagonal *adapter* layer for the mempool subsystem. It
//! supplies the runtime machinery that drives the foundational core defined in
//! [`zaino-mempool`](zaino_mempool): the polling [`MempoolService`] (which fills
//! and bounds the mempool set from a [`zaino_mempool::MempoolSource`]) and the
//! lock-free [`MempoolSubscriber`] read handle.
//!
//! Dependencies point inward: this crate depends on `zaino-mempool` (the ports +
//! foundational types); `zaino-mempool` never names anything here.

pub mod service;
pub mod subscriber;

#[cfg(test)]
mod tests;

pub use service::MempoolService;
pub use subscriber::{MempoolFilterError, MempoolInfo, MempoolSubscriber, TxIdExcludeSuffix};
