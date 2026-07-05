//! Process-level rustls `CryptoProvider` management.

/// Installs rustls's `aws-lc-rs` provider as the process-level default
/// if no provider is installed yet.
///
/// Call this before constructing anything that builds a rustls config —
/// a `reqwest` client (the workspace enables reqwest's
/// `rustls-no-provider` feature, which never auto-selects a provider) or
/// a TLS-enabled tonic server. rustls's own auto-selection is also
/// unreliable here: it works only when exactly one of rustls's `ring` /
/// `aws-lc-rs` features is enabled, and dependency feature-unification
/// can silently enable both, which panicked at runtime on TLS-enabled
/// deployments (zingolabs/zaino#1360). Installing explicitly removes
/// that dependence on the feature graph.
///
/// aws-lc-rs is the preferred provider (ADR-0006): it is the only rustls
/// provider with post-quantum key exchange, and the workspace enables
/// rustls's `prefer-post-quantum` feature so the X25519MLKEM768 hybrid
/// group leads this provider's defaults. That ordering governs zaino's
/// outbound (client-role) handshakes; rustls servers follow the client's
/// group preference, so inbound hybrid uptake is decided by the
/// connecting wallet's TLS stack. Classical key-exchange groups remain
/// offered and accepted, but are deprecated (see the ADR).
///
/// The process default is first-install-wins and is not constrained by
/// our crates' rustls features, so an embedder (e.g. zallet) that has
/// already installed a provider keeps it: zaino then handshakes through
/// that provider instead of aws-lc-rs, which is fine for any provider
/// implementing the standard TLS suites (ring and aws-lc-rs both do).
pub fn ensure_default_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // A racing concurrent install is the only error case; either
        // provider serves.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
