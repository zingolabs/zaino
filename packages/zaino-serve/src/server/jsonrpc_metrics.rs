//! JSON-RPC serving metrics, as a jsonrpsee RPC-layer middleware.
//!
//! Middleware, not per-handler timers:
//! - gRPC has `implement_client_methods!`; `#[rpc]` has no such chokepoint, so
//!   this would be ~40 edits + 40 chances to miss the 41st method
//! - Sits post-parse, post-split → sees resolved name & finished response for
//!   every call, same seam as `FixRpcResponseMiddleware`
//!
//! Label interned against the server's method table:
//! - Seam is outside `RpcService` (method lookup), so unknown methods arrive
//!   carrying caller-controlled strings
//! - Recorder = one series per distinct label value, never evicted; each costs a
//!   bucketed histogram + an error counter → a random-method loop is a remote OOM
//!   of the *indexer*, over an endpoint with no TLS and maybe no auth
//! - Registered name → its `&'static str`, everything else → [`UNKNOWN_METHOD`]:
//!   cardinality bounded by the API surface, no per-request allocation, and a
//!   spike on `method="unknown"` is the actionable form anyway
//!
//! `is_error` = handler error *or* framework rejection (unknown method, bad
//! params); both are caller-side failures.

use std::{collections::HashSet, sync::Arc};

use jsonrpsee::{
    server::middleware::rpc::{layer::ResponseFuture, RpcServiceT},
    MethodResponse,
};

use crate::metric_names::{JSONRPC_ERRORS_TOTAL, JSONRPC_REQUEST_DURATION_SECONDS, SERVE_METHOD};

/// Label for any call naming an unregistered method — one series, forever.
pub(crate) const UNKNOWN_METHOD: &str = "unknown";

/// Method names this server labels by name.
///
/// - `&'static str`: what jsonrpsee's table already holds (`#[rpc]` registers
///   literals), so interning yields an allocation-free label
pub(crate) type MethodNames = Arc<HashSet<&'static str>>;

/// Records serving latency and error counts for every JSON-RPC call.
#[derive(Clone)]
pub(crate) struct MetricsMiddleware<S> {
    service: S,
    methods: MethodNames,
}

impl<S> MetricsMiddleware<S> {
    /// Wrap `service`, labelling calls against `methods`.
    pub(crate) fn new(service: S, methods: MethodNames) -> Self {
        Self { service, methods }
    }
}

/// Resolve `name` to its recorded label.
///
/// - A function because it *is* the cardinality bound; testing it through the
///   middleware would need a live `jsonrpsee` service and request
fn label_for(methods: &MethodNames, name: &str) -> &'static str {
    methods.get(name).copied().unwrap_or(UNKNOWN_METHOD)
}

impl<'a, S> RpcServiceT<'a> for MetricsMiddleware<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = ResponseFuture<futures::future::BoxFuture<'a, MethodResponse>>;

    fn call(&self, request: jsonrpsee::types::Request<'a>) -> Self::Future {
        let service = self.service.clone();
        // Before the await: `request` borrows the connection buffer and is moved
        // into the call
        let method = label_for(&self.methods, request.method_name());

        ResponseFuture::future(Box::pin(async move {
            let started = std::time::Instant::now();
            let response = service.call(request).await;

            // `_count` = request volume, so no separate counter — same shape as
            // the gRPC side
            metrics::histogram!(JSONRPC_REQUEST_DURATION_SECONDS, SERVE_METHOD => method)
                .record(started.elapsed().as_secs_f64());

            if response.is_error() {
                // Code is in the series (zcashd clients branch on it) and bounded
                // — produced by zaino & jsonrpsee, never the caller. Sentinel on
                // absent keeps the shape stable rather than dropping the sample
                let code = response.as_error_code().unwrap_or(0);
                metrics::counter!(
                    JSONRPC_ERRORS_TOTAL,
                    SERVE_METHOD => method,
                    "code" => code.to_string(),
                )
                .increment(1);
            }

            response
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> MethodNames {
        Arc::new(HashSet::from(["getblock", "getinfo"]))
    }

    /// - Dashboard label must match the method the client called
    #[test]
    fn a_registered_method_is_labelled_by_name() {
        assert_eq!(label_for(&registered(), "getblock"), "getblock");
        assert_eq!(label_for(&registered(), "getinfo"), "getinfo");
    }

    /// - Recorder keys series by label *value*, so one value = finite series;
    ///   the `len() == 1` assert is the bound itself, not a proxy
    /// - Shapes an attacker reaches for: near-miss, case, whitespace, non-names
    #[test]
    fn no_unregistered_method_can_mint_a_label() {
        let methods = registered();
        let mut labels = std::collections::HashSet::new();
        for name in [
            "getblokc",
            "",
            "GETBLOCK",
            "getblock ",
            "a-very-long-name-a-client-made-up",
            "{\"injected\":\"json\"}",
        ] {
            let label = label_for(&methods, name);
            assert_eq!(label, UNKNOWN_METHOD, "`{name}` leaked into a metric label");
            labels.insert(label);
        }
        assert_eq!(
            labels.len(),
            1,
            "unregistered methods produced more than one label value, so each \
             would become its own time series"
        );
    }
}
