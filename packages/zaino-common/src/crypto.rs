//! Process-level rustls `CryptoProvider` management.

/// Installs rustls's `ring` provider as the process-level default if no
/// provider is installed yet.
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
/// The process default is first-install-wins and is not constrained by
/// our crates' rustls features, so an embedder (e.g. zallet) that has
/// already installed a provider keeps it: zaino then handshakes through
/// that provider instead of ring, which is fine for any provider
/// implementing the standard TLS suites (ring and aws-lc-rs both do).
pub fn ensure_default_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // A racing concurrent install is the only error case; either
        // provider serves.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
