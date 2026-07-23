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
//! backend's surface is a superset of the validator's**. `PassthroughSource`
//! names *exactly the validator-serviceable subset* — the reads the runtime can
//! satisfy from the validator when the backend can't (or doesn't store them).
//! Passthrough is nothing more than sourcing that subset from the validator
//! instead of the backend.
//!
//! Modelling it as a named subset buys locality of correctness for free: a
//! **synthetic** capability has no port in this subset, so the runtime *cannot*
//! wire a validator fallback for it — that read is `NotServiceable`-when-local-
//! can't-serve by construction, not by a runtime check. The subset grows one
//! supertrait at a time as more direct capabilities are wired.
//!
//! Reads are **by hash / immutable id**: a hash is reorg-stable, so the answer
//! is coherent even inside a pinned snapshot (a live validator read *by height*
//! against a moving chain is the torn-read hazard we avoid — that degradation
//! belongs to the live/latest layer, not the pinned view).

use zaino_source::{GetBlockByHash, GetTransaction};

/// The validator-serviceable subset of the indexer's read capabilities.
///
/// A bundle of `zaino-source` driven ports, with a blanket impl so any
/// validator adapter that provides the underlying ports is automatically a
/// `PassthroughSource`. Add a supertrait here as each further **direct**
/// capability is wired for passthrough; **synthetic** capabilities are, by
/// design, absent.
pub trait PassthroughSource: GetBlockByHash + GetTransaction {}

impl<T: GetBlockByHash + GetTransaction> PassthroughSource for T {}
