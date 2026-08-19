//! `getpeerinfo` — the backing validator's peer connections.

/// One peer connection held by the backing validator.
///
/// These are the *validator's* peers. Zaino is not a p2p node and has none of
/// its own; it forwards this listing on the validator's behalf.
///
/// # Why every field is required
///
/// Zebra reports exactly these two fields. Richer per-peer data — protocol
/// version, user agent, byte counters, sync progress, ban score — is reported
/// only by zcashd, which is being deprecated. Modelling those here would mean
/// carrying roughly twenty fields that are permanently `None` against the
/// supported validator, and encoding a deprecated implementation's internals
/// into Zaino's vocabulary.
///
/// If a validator and a consumer ever jointly justify the richer listing, it
/// belongs in a separate `GetPeerInfoDetailed` source trait returning its own
/// fully-populated type — capability expressed by which traits an adapter
/// implements, matching how the rest of the source layer works. That keeps both
/// types free of optional fields and needs no parse-time guess about which
/// shape arrived.
///
/// # Why this is not an enum over response shapes
///
/// The previous wire type discriminated zcashd from zebrad responses by trying
/// each strict (`deny_unknown_fields`) struct in turn and falling back to
/// untyped JSON. Strictness *was* the discriminator, which made it fail in
/// three ways: a real zcashd response carrying one unrecognised field silently
/// degraded to untyped passthrough, Zebra adding a third field would do the
/// same to the primary supported validator, and richness is a property of the
/// validator yet was encoded per response. A single lenient shape has no
/// discriminator to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Remote peer address as the validator reports it.
    ///
    /// Opaque on purpose. This is usually `host:port`, but Zcash peers are
    /// routinely reached over Tor or I2P, whose addresses are not socket
    /// addresses, and a validator may report a hostname. Zaino never inspects
    /// this value — it forwards it — so parsing it here could only reject a
    /// valid peer. A consumer that needs the structured form can parse at its
    /// own call site.
    pub addr: String,

    /// Whether the peer initiated the connection to the validator.
    pub inbound: bool,
}
