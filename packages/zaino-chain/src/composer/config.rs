//! Deployment choices.

/// How a chain view is configured.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ChainViewConfig {
    /// Whether the validator may be consulted.
    ///
    /// Off means a range the providers do not cover becomes unserviceable
    /// rather than filled, and reads no provider holds at all — raw
    /// transactions, treestates, address history — stop working. A deployment
    /// that must not talk to its validator on the read path accepts serving
    /// only what it holds.
    pub passthrough_enabled: bool,

    /// Whether the finalised store contributes coverage.
    ///
    /// Off means the chain head and the validator answer everything. This
    /// replaces the ephemeral mode that used to live inside the store.
    ///
    /// Distinct from a store that is merely *behind*: a store still building
    /// contributes the prefix it has built, and the range above it is filled.
    pub store_enabled: bool,

    /// The most validator fetches in flight at once, across every client.
    ///
    /// # What this bounds, and what it does not
    ///
    /// It bounds *validator load*, not client count. Store reads are never
    /// gated — the normal path, once a deployment has caught up, never touches
    /// this at all. A client waiting for a permit is a parked task costing a
    /// few hundred bytes, so thousands of clients can share a few hundred
    /// permits without any of them being blocked in any meaningful sense: they
    /// queue inside Zaino instead of queueing inside the validator, which is
    /// the whole point.
    ///
    /// The permit is taken **per block fetched**, never held for the length of
    /// a stream. A client streaming a million blocks holds a permit only while
    /// each individual block is in flight, so one long sync cannot starve
    /// anyone.
    ///
    /// # Why this exists
    ///
    /// Without it, concurrency is the product of clients and per-client
    /// concurrency: 5,000 clients each fetching 16 blocks at once is 80,000
    /// simultaneous requests. Nothing else in the stack caps this — there is no
    /// limit in `zaino-rpc`, the source adapters, or the daemon config — so a
    /// catch-up deployment under load would exhaust sockets or overwhelm the
    /// validator, and only under load.
    ///
    /// # The default
    ///
    /// 512.
    ///
    /// Throughput is `permits / latency`: at a 5 ms validator round trip that
    /// is roughly 100,000 block fetches per second, which saturates any
    /// realistic gap-fill demand from 5,000–10,000 syncing clients while
    /// keeping concurrent connections to one validator within what a JSON-RPC
    /// server handles comfortably.
    ///
    /// Note when sizing: one permit covers one *block*, which for a compact
    /// block is two requests issued together (the projection and its tree
    /// sizes), so in-flight requests peak at about twice this.
    pub passthrough_permits: usize,

    /// The most validator fetches one request may have in flight.
    ///
    /// A second, per-request bound beneath [`Self::passthrough_permits`], so a
    /// single large range cannot occupy every permit and stall every other
    /// client behind it. Small on purpose: the shared pool provides the
    /// throughput, this provides the fairness.
    pub passthrough_per_request: usize,

    /// How many bytes a streamed chunk aims for.
    ///
    /// Chunks are sized by bytes rather than by block count because block sizes
    /// vary by orders of magnitude across the chain, and a fixed count would
    /// make memory per client swing with it. The walk starts small — so
    /// latency to first byte stays low — and adapts toward this budget from
    /// what it has actually seen.
    pub chunk_budget_bytes: usize,
}

impl Default for ChainViewConfig {
    fn default() -> Self {
        Self {
            passthrough_enabled: true,
            store_enabled: true,
            passthrough_permits: 512,
            passthrough_per_request: 16,
            chunk_budget_bytes: 2 * 1024 * 1024,
        }
    }
}

impl ChainViewConfig {
    /// Refuses reads that would reach the validator.
    pub fn without_passthrough(mut self) -> Self {
        self.passthrough_enabled = false;
        self
    }

    /// Ignores the finalised store.
    pub fn without_store(mut self) -> Self {
        self.store_enabled = false;
        self
    }

    /// Sets the shared validator-fetch bound.
    pub fn with_passthrough_permits(mut self, permits: usize) -> Self {
        self.passthrough_permits = permits.max(1);
        self
    }

    /// Sets the per-request validator-fetch bound.
    pub fn with_passthrough_per_request(mut self, per_request: usize) -> Self {
        self.passthrough_per_request = per_request.max(1);
        self
    }

    /// Sets the streamed-chunk byte budget.
    pub fn with_chunk_budget_bytes(mut self, bytes: usize) -> Self {
        self.chunk_budget_bytes = bytes.max(1);
        self
    }
}
