//! The zcashd/bitcoind JSON-RPC **1.0 dialect** HTTP layer.
//!
//! jsonrpsee speaks strict JSON-RPC 2.0, but the explorer's `zcashex` client
//! speaks the zcashd/bitcoind dialect: `"jsonrpc": "1.0"` (which jsonrpsee rejects
//! with -32600), a response envelope carrying **both** a `result` and an `error`
//! key, and the HTTP status code read to tell success from failure. This
//! [`tower::Layer`]/[`tower::Service`], installed via
//! [`set_http_middleware`](jsonrpsee::server::ServerBuilder::set_http_middleware),
//! bridges that gap: it classifies and rewrites the request in, and reshapes the
//! response out.
//!
//! The four [`Dialect`]s and the full wire contract — classification, the
//! always-both-keys envelope, and the error-to-HTTP-status mapping — are the
//! crate's wire policy, in `usage.md` ("Wire dialects"). The classification
//! re-derives Zebra's `HttpRequestMiddleware` so this adapter stays
//! validator-agnostic and takes **no** `zebra-rpc` dependency.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::header::{self, HeaderMap, HeaderValue};
use http::StatusCode;
use http_body_util::{BodyExt, Limited};
use jsonrpsee::core::BoxError;
use jsonrpsee::server::{HttpBody, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tower::{Layer, Service};

/// The JSON-RPC bitcoind error code for an unknown method, mapped to HTTP 404.
const METHOD_NOT_FOUND: i64 = -32601;
/// The JSON-RPC bitcoind error code for an invalid request, mapped to HTTP 400.
const INVALID_REQUEST: i64 = -32600;

/// The JSON-RPC dialect a request is speaking.
///
/// Classified exactly as Zebra classifies it; the names are Zebra's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// bitcoind's mishmash of 1.0/1.1/2.0, recognised by an **absent** `jsonrpc`
    /// field with both `params` and `id` present.
    Bitcoind,
    /// lightwalletd's dialect — bitcoind's, but breaking spec with an explicit
    /// `"jsonrpc": "1.0"`. This is the dialect `zcashex` uses.
    Lightwalletd,
    /// Strict JSON-RPC 2.0.
    TwoPointZero,
    /// Anything else (including batch arrays and unparseable bodies): passed
    /// through untouched for jsonrpsee to handle.
    Unknown,
}

/// A version-agnostic JSON-RPC request, parsed only far enough to classify and
/// rewrite the `jsonrpc` field.
#[derive(Debug, Deserialize, Serialize)]
struct JsonRpcRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<String>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Classify the dialect from the `(jsonrpc, params, id)` triple, mirroring
    /// Zebra's `JsonRpcRequest::version`.
    fn dialect(&self) -> Dialect {
        match (self.jsonrpc.as_deref(), &self.params, &self.id) {
            (
                Some("2.0"),
                _,
                None
                | Some(
                    serde_json::Value::Null
                    | serde_json::Value::String(_)
                    | serde_json::Value::Number(_),
                ),
            ) => Dialect::TwoPointZero,
            (Some("1.0"), Some(_), Some(_)) => Dialect::Lightwalletd,
            (None, Some(_), Some(_)) => Dialect::Bitcoind,
            _ => Dialect::Unknown,
        }
    }

    /// Rewrite the request to strict 2.0 so jsonrpsee accepts it.
    fn into_2(mut self) -> Self {
        self.jsonrpc = Some("2.0".to_owned());
        self
    }
}

/// A version-agnostic JSON-RPC response. `result` is kept as a [`RawValue`] so a
/// backend payload is echoed byte-for-byte, without a number-precision round
/// trip.
#[derive(Debug, Deserialize, Serialize)]
struct JsonRpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<String>,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// The bitcoind/zcashd HTTP status for this response: 200 on success, and on
    /// error the status bitcoind's `JSONErrorReply` assigns from the JSON-RPC
    /// error code (see `httpserver.cpp` / `rpc/protocol.h` in the zcashd lineage):
    /// method-not-found (-32601) → 404, invalid-request (-32600) → 400, every
    /// other error — parse errors, invalid params (-32602), internal errors → 500.
    ///
    /// Computed from the as-parsed 2.0 response, before [`Self::into_dialect`]
    /// fills in the null keys.
    fn http_status(&self) -> StatusCode {
        match &self.error {
            Some(error) if !error.is_null() => match error_code(error) {
                Some(METHOD_NOT_FOUND) => StatusCode::NOT_FOUND,
                Some(INVALID_REQUEST) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            _ => StatusCode::OK,
        }
    }

    /// Reshape the 2.0 response into the given legacy dialect: set `jsonrpc`
    /// (`"1.0"` for Lightwalletd, absent for Bitcoind) and emit both `result` and
    /// `error` keys, filling the absent one with `null`.
    fn into_dialect(mut self, dialect: Dialect) -> Result<Self, serde_json::Error> {
        self.jsonrpc = match dialect {
            Dialect::Lightwalletd => Some("1.0".to_owned()),
            Dialect::Bitcoind => None,
            // The caller only reshapes the two legacy dialects; the others are
            // returned untouched before reaching here.
            Dialect::TwoPointZero | Dialect::Unknown => return Ok(self),
        };
        if self.result.is_none() {
            self.result = Some(serde_json::value::to_raw_value(&serde_json::Value::Null)?);
        }
        if self.error.is_none() {
            self.error = Some(serde_json::Value::Null);
        }
        Ok(self)
    }
}

/// Extract the numeric `code` from a JSON-RPC error object.
fn error_code(error: &serde_json::Value) -> Option<i64> {
    error.get("code").and_then(serde_json::Value::as_i64)
}

/// A failure reshaping a request or response at the dialect boundary. Each keeps
/// its cause via `#[source]`.
#[derive(Debug, thiserror::Error)]
enum DialectError {
    /// The request body could not be read (an I/O failure or the body exceeding
    /// the configured size limit).
    #[error("failed to read the JSON-RPC request body")]
    ReadRequestBody(#[source] BoxError),
    /// The backend response body could not be read.
    #[error("failed to read the JSON-RPC response body")]
    ReadResponseBody(#[source] BoxError),
    /// A classified legacy request could not be re-encoded as 2.0.
    #[error("failed to re-encode the JSON-RPC request as 2.0")]
    EncodeRequest(#[source] serde_json::Error),
    /// The backend response was not valid JSON-RPC 2.0 — jsonrpsee always emits
    /// valid 2.0, so this is a server-side fault, not client input.
    #[error("failed to parse the backend JSON-RPC response")]
    ParseResponse(#[source] serde_json::Error),
    /// The reshaped dialect response could not be re-encoded.
    #[error("failed to re-encode the dialect JSON-RPC response")]
    EncodeResponse(#[source] serde_json::Error),
}

/// Force the `content-type` to `application/json` when it is absent or
/// `text/plain` — jsonrpsee does no content sniffing, and `zcashex` sends
/// `text/plain`. Any other explicit type is left as the client set it.
fn insert_or_replace_content_type(headers: &mut HeaderMap) {
    let replace = match headers.get(header::CONTENT_TYPE) {
        None => true,
        Some(value) => value
            .to_str()
            .map(|value| value.starts_with("text/plain"))
            .unwrap_or(false),
    };
    if replace {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
}

/// Classify the request and, for a legacy dialect, rewrite its body to 2.0.
///
/// A body that parses as a single request but is not a legacy dialect, and a
/// body that does not parse at all (a batch array, or garbage), are both
/// [`Dialect::Unknown`] and pass through with their original bytes.
async fn request_to_2(
    request: HttpRequest<HttpBody>,
    max_request_body_size: usize,
) -> Result<(Dialect, HttpRequest<HttpBody>), DialectError> {
    let (parts, body) = request.into_parts();
    let bytes = Limited::new(body, max_request_body_size)
        .collect()
        .await
        .map_err(DialectError::ReadRequestBody)?
        .to_bytes();
    let (dialect, bytes) = match serde_json::from_slice::<JsonRpcRequest>(bytes.as_ref()) {
        Ok(request) => {
            let dialect = request.dialect();
            match dialect {
                Dialect::Lightwalletd | Dialect::Bitcoind => (
                    dialect,
                    serde_json::to_vec(&request.into_2()).map_err(DialectError::EncodeRequest)?,
                ),
                Dialect::TwoPointZero | Dialect::Unknown => (dialect, bytes.as_ref().to_vec()),
            }
        }
        Err(_) => (Dialect::Unknown, bytes.as_ref().to_vec()),
    };
    Ok((
        dialect,
        HttpRequest::from_parts(parts, HttpBody::from(bytes)),
    ))
}

/// Reshape a 2.0 response back into the request's legacy dialect, mapping the
/// HTTP status. 2.0 and Unknown pass through untouched.
async fn response_from_2(
    dialect: Dialect,
    response: HttpResponse<HttpBody>,
) -> Result<HttpResponse<HttpBody>, DialectError> {
    match dialect {
        Dialect::TwoPointZero | Dialect::Unknown => return Ok(response),
        Dialect::Lightwalletd | Dialect::Bitcoind => {}
    }
    let (mut parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(DialectError::ReadResponseBody)?
        .to_bytes();
    let parsed: JsonRpcResponse =
        serde_json::from_slice(bytes.as_ref()).map_err(DialectError::ParseResponse)?;
    parts.status = parsed.http_status();
    let encoded = serde_json::to_vec(
        &parsed
            .into_dialect(dialect)
            .map_err(DialectError::EncodeResponse)?,
    )
    .map_err(DialectError::EncodeResponse)?;
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(HttpResponse::from_parts(parts, HttpBody::from(encoded)))
}

/// A [`tower::Layer`] installing the zcashd-dialect bridge over a jsonrpsee HTTP
/// service.
#[derive(Clone)]
pub(crate) struct ZcashdDialectLayer {
    max_request_body_size: usize,
}

impl ZcashdDialectLayer {
    /// A layer limiting request bodies to `max_request_body_size` bytes.
    pub(crate) fn new(max_request_body_size: usize) -> Self {
        Self {
            max_request_body_size,
        }
    }
}

impl<S> Layer<S> for ZcashdDialectLayer {
    type Service = ZcashdDialect<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ZcashdDialect {
            inner,
            max_request_body_size: self.max_request_body_size,
        }
    }
}

/// The [`tower::Service`] produced by [`ZcashdDialectLayer`].
#[derive(Clone)]
pub(crate) struct ZcashdDialect<S> {
    inner: S,
    max_request_body_size: usize,
}

impl<S> Service<HttpRequest<HttpBody>> for ZcashdDialect<S>
where
    S: Service<HttpRequest, Response = HttpResponse> + Clone + Send + 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: HttpRequest<HttpBody>) -> Self::Future {
        insert_or_replace_content_type(request.headers_mut());
        // Drive the clone that was made ready by `poll_ready`, leaving a fresh
        // clone in its place for the next readiness check.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let max_request_body_size = self.max_request_body_size;
        Box::pin(async move {
            let (dialect, request) = request_to_2(request, max_request_body_size).await?;
            let response = inner.call(request).await.map_err(Into::into)?;
            Ok(response_from_2(dialect, response).await?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Dialect, JsonRpcRequest};

    fn classify(body: &str) -> Dialect {
        serde_json::from_str::<JsonRpcRequest>(body)
            .map(|request| request.dialect())
            .unwrap_or(Dialect::Unknown)
    }

    /// The classifier recognises each dialect from its `(jsonrpc, params, id)`
    /// shape, exactly as Zebra does, and treats everything else — including a
    /// batch array — as Unknown.
    #[test]
    fn classifier_recognises_each_dialect() {
        // zcashex: explicit "1.0", params and id present.
        assert_eq!(
            classify(r#"{"jsonrpc":"1.0","id":"zcashex","method":"getblockcount","params":[]}"#),
            Dialect::Lightwalletd,
        );
        // bitcoind: no jsonrpc field, params and id present.
        assert_eq!(
            classify(r#"{"id":1,"method":"getblockcount","params":[]}"#),
            Dialect::Bitcoind,
        );
        // strict 2.0.
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}"#),
            Dialect::TwoPointZero,
        );
        // A 2.0 notification (no id) is still 2.0.
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","method":"getblockcount","params":[]}"#),
            Dialect::TwoPointZero,
        );
        // "1.0" without params/id does not match the legacy pattern.
        assert_eq!(
            classify(r#"{"jsonrpc":"1.0","id":1,"method":"getblockcount"}"#),
            Dialect::Unknown,
        );
        // A batch array does not parse as one request: Unknown, passed through.
        assert_eq!(
            classify(r#"[{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}]"#),
            Dialect::Unknown,
        );
        // Garbage: Unknown.
        assert_eq!(classify("not json at all"), Dialect::Unknown);
    }
}
