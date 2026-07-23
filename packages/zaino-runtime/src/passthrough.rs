//! The passthrough surface.
//!
//! The indexer promises consumers a read-capability set. Those capabilities
//! have three provenances:
//!
//! - **Direct** — derivable straight from chain data (block by hash, raw tx,
//!   treestate, tip). Both the local backend *and* the validator can answer.
//! - **Synthetic** — produced *by indexing* (address history, spend index,
//!   txid→location, ChainMetadata tree sizes). Only the local backend can
//!   answer; the validator holds no such index.
//! - **Not-locally-stored** — in the promise, but the backend chose not to
//!   cache (full `Block`s, raw tx bytes). The validator answers.
//!
//! So there are two providers over one read vocabulary, and the **local
//! backend's surface is a superset of the validator's**. Passthrough serves
//! *exactly the validator-serviceable subset* from the validator when the
//! backend can't (or doesn't store the data).
//!
//! **This port is shaped in the domain's terms** — the same questions the
//! domain asks of its persistence layer (domain types, domain keys, a domain
//! error), *not* the validator's transport shape. From the domain's view the
//! validator is simply a slower persistence backend. A **validator adapter**
//! (a separate crate, over `zaino-source`) implements this port by wrapping an
//! RPC client and translating transport results/errors — and raw tx bytes — to
//! domain types. The runtime never sees the transport surface; it composes over
//! this domain port only.
//!
//! Modelling the subset as a named port buys locality of correctness for free:
//! a **synthetic** capability has no method here, so the runtime *cannot* wire
//! a validator fallback for it — that read is `NotServiceable`-when-local-can't
//! by construction. The port grows one method at a time as more direct
//! capabilities are wired.
//!
//! Reads are **by hash / immutable id**: a hash is reorg-stable, so the answer
//! is coherent even inside a pinned snapshot (a live validator read *by height*
//! against a moving chain is the torn-read hazard we avoid — that degradation
//! belongs to the live/latest layer, not the pinned view).

use std::future::Future;

use zaino_core::{Block, BlockHash, Transaction, TransactionHash};

/// The validator-serviceable subset of the indexer's read capabilities,
/// expressed as domain reads. Implemented by a validator adapter over
/// `zaino-source`; consumed by the runtime as a plain domain port.
pub trait PassthroughSource: Send + Sync {
    /// Fetch a full block by hash. `None` if the validator has no such block.
    fn block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Block>, PassthroughError>> + Send;

    /// Fetch a transaction by txid. `None` if the validator has no such tx.
    fn transaction(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<Transaction>, PassthroughError>> + Send;
}

/// A passthrough read could not be answered.
///
/// Transport *mechanism* — connection errors, timeouts, RPC codes, retry and
/// backoff — is handled **below** this port, in the validator adapter's
/// resilience layer (the anti-corruption layer over the `zaino-source`
/// gateway). Only the domain-classified residue crosses the port: by here,
/// retries are already spent and no `HttpStatus`/`FailureMode` remains. The
/// runtime projects this onto its serving contract (→ `Transient` or
/// `NotServiceable`) as *serving policy* — it never handles transport itself.
#[derive(Debug)]
pub enum PassthroughError {
    /// The validator was unreachable after the adapter's best effort. The
    /// string is an **opaque diagnostic for logging only** — domain/runtime
    /// logic must not branch on its contents.
    Unavailable(String),
}
