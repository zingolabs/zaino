//! JSON-RPC serving metrics, as a jsonrpsee RPC-layer middleware
//!
//! - Middleware, not per-handler timers: `#[rpc]` has no macro chokepoint to hook
//! - Labels interned against the method table: callers name methods, the recorder
//!   never evicts a series, so a random-method loop would OOM the indexer

use std::{collections::HashSet, sync::Arc};

use jsonrpsee::{
    server::middleware::rpc::{layer::ResponseFuture, RpcServiceT},
    MethodResponse,
};

use crate::metric_names::{
    JSONRPC_ERRORS_TOTAL, JSONRPC_REQUEST_DURATION_SECONDS, SERVE_CODE, SERVE_METHOD,
};

/// Label for any call naming an unregistered method — one series, forever
pub(crate) const UNKNOWN_METHOD: &str = "unknown";

/// Method names this server labels by name
///
/// - `&'static str`: what jsonrpsee's table already holds, so interning is alloc-free
pub(crate) type MethodNames = Arc<HashSet<&'static str>>;

/// Records serving latency and error counts for every JSON-RPC call
#[derive(Clone)]
pub(crate) struct MetricsMiddleware<S> {
    service: S,
    methods: MethodNames,
}

impl<S> MetricsMiddleware<S> {
    /// Wrap `service`, labelling calls against `methods`
    pub(crate) fn new(service: S, methods: MethodNames) -> Self {
        Self { service, methods }
    }
}

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

            metrics::histogram!(JSONRPC_REQUEST_DURATION_SECONDS, SERVE_METHOD => method)
                .record(started.elapsed().as_secs_f64());

            if response.is_error() {
                // Bounded: produced by zaino & jsonrpsee, never the caller. Sentinel
                // on absent keeps the series shape stable
                let code = response.as_error_code().unwrap_or(0);
                metrics::counter!(
                    JSONRPC_ERRORS_TOTAL,
                    SERVE_METHOD => method,
                    SERVE_CODE => code.to_string(),
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

    /// - `len() == 1` is the cardinality bound itself, not a proxy for it
    /// - Shapes an attacker reaches for: near-miss, case, whitespace, non-names
    #[test]
    fn no_unregistered_method_can_mint_a_label() {
        let methods = registered();
        assert_eq!(label_for(&methods, "getblock"), "getblock");

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
        assert_eq!(labels.len(), 1, "each would become its own time series");
    }
}
