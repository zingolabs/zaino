//! What this crate asks of one validator.
//!
//! - `getbestblockheightandhash` — readiness: the only port that can say `NotReady`
//! - `getrawmempool true` — the listing the diff runs over, each entry's fee included (the
//!   validator resolved its prevouts admitting it)
//! - `getrawtransaction(txid, 0)` — bytes for what the diff added, fetched once per txid
//! - the tip *of the mempool's own source* — this endpoint's quorum vote, coherent with the
//!   listing it was read beside
//! - `sendrawtransaction` — the broadcast fan-out (§5)
//! - `getpeerinfo` — each validator's peers (the graph discovery traverses)
//!
//! - Retry = this crate's own per-endpoint ladder (`config.rs`)

/// Every question the endpoint poller and the broadcast fan-out ask a validator.
///
/// Blanket-impl'd, so a production adapter and a test fake earn it the same way.
///
/// Not `Clone`: an adapter may own connections. Shared behind an `Arc`.
pub trait EndpointSource:
    zaino_source::GetChainTip
    + zaino_source::GetMempoolListing
    + zaino_source::GetRawMempoolTransaction
    + zaino_source::GetMempoolSourceTip
    + zaino_source::SendRawTransaction
    + zaino_source::GetPeerInfo
    + Send
    + Sync
    + 'static
{
}

impl<T> EndpointSource for T where
    T: zaino_source::GetChainTip
        + zaino_source::GetMempoolListing
        + zaino_source::GetRawMempoolTransaction
        + zaino_source::GetMempoolSourceTip
        + zaino_source::SendRawTransaction
        + zaino_source::GetPeerInfo
        + Send
        + Sync
        + 'static
{
}
