use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use http_body_util::{combinators::Collect, BodyExt as _, Limited};
use hyper::header;
use jsonrpsee::{
    core::BoxError,
    server::{HttpBody, HttpRequest, HttpResponse},
    types::ErrorObject,
};
use serde::{Deserialize, Serialize};
use tower::Service;

use super::cookie::Cookie;

/// An HTTP middleware that authenticates the cookie, defaults the content type to JSON, and speaks each client's JSON-RPC dialect as 2.0.
#[derive(Clone, Debug)]
pub(crate) struct HttpRequestMiddleware<S> {
    service: S,
    cookie: Option<Cookie>,
    max_request_body_size: usize,
}

impl<S> HttpRequestMiddleware<S> {
    /// Wraps `service`, requiring `cookie` when one is given and refusing bodies over `max_request_body_size` bytes.
    pub(crate) fn new(service: S, cookie: Option<Cookie>, max_request_body_size: usize) -> Self {
        Self {
            service,
            cookie,
            max_request_body_size,
        }
    }

    /// Whether the request's basic-auth password is the cookie, or no cookie is required.
    fn check_credentials(&self, headers: &header::HeaderMap) -> bool {
        self.cookie.as_ref().is_none_or(|internal_cookie| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|auth_header| auth_header.to_str().ok())
                .and_then(|auth_header| auth_header.split_whitespace().nth(1))
                .and_then(|encoded| STANDARD.decode(encoded).ok())
                .and_then(|decoded| String::from_utf8(decoded).ok())
                .and_then(|request_cookie| request_cookie.split(':').nth(1).map(String::from))
                .is_some_and(|passwd| internal_cookie.authenticate(passwd))
        })
    }
}

/// Sets the content type to JSON when it is absent or `text/plain`, and leaves any other type, such as a form encoding, for jsonrpsee to reject.
fn insert_or_replace_content_type_header(headers: &mut header::HeaderMap) {
    if !headers.contains_key(header::CONTENT_TYPE)
        || headers
            .get(header::CONTENT_TYPE)
            .filter(|value| {
                value
                    .to_str()
                    .ok()
                    .unwrap_or_default()
                    .starts_with("text/plain")
            })
            .is_some()
    {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
    }
}

/// Rewrites a request body in the client's JSON-RPC dialect as a 2.0 request, reporting the dialect.
fn request_to_json_rpc_2(
    parts: http::request::Parts,
    bytes: Bytes,
) -> (JsonRpcVersion, HttpRequest<HttpBody>) {
    let (version, bytes) =
        if let Ok(request) = serde_json::from_slice::<'_, JsonRpcRequest>(bytes.as_ref()) {
            let version = request.version();
            if matches!(version, JsonRpcVersion::Unknown) {
                (version, bytes)
            } else {
                (
                    version,
                    serde_json::to_vec(&request.into_2())
                        .expect("a parsed JSON-RPC request re-serializes")
                        .into(),
                )
            }
        } else {
            (JsonRpcVersion::Unknown, bytes)
        };
    (
        version,
        HttpRequest::from_parts(parts, HttpBody::from(bytes.as_ref().to_vec())),
    )
}

/// Rewrites a 2.0 response body in the client's JSON-RPC dialect.
fn response_from_json_rpc_2(
    version: JsonRpcVersion,
    parts: http::response::Parts,
    bytes: Bytes,
) -> HttpResponse<HttpBody> {
    let bytes = if let Ok(response) = serde_json::from_slice::<'_, JsonRpcResponse>(bytes.as_ref())
    {
        serde_json::to_vec(&response.into_version(version))
            .expect("a parsed JSON-RPC response re-serializes")
            .into()
    } else {
        bytes
    };
    HttpResponse::from_parts(parts, HttpBody::from(bytes.as_ref().to_vec()))
}

/// A [`tower::Layer`] that wraps a service in [`HttpRequestMiddleware`].
#[derive(Clone)]
pub(crate) struct HttpRequestMiddlewareLayer {
    cookie: Option<Cookie>,
    max_request_body_size: usize,
}

impl HttpRequestMiddlewareLayer {
    /// Creates the layer, requiring `cookie` when one is given and refusing bodies over `max_request_body_size` bytes.
    pub(crate) fn new(cookie: Option<Cookie>, max_request_body_size: usize) -> Self {
        Self {
            cookie,
            max_request_body_size,
        }
    }
}

impl<S> tower::Layer<S> for HttpRequestMiddlewareLayer {
    type Service = HttpRequestMiddleware<S>;

    fn layer(&self, service: S) -> Self::Service {
        HttpRequestMiddleware::new(service, self.cookie.clone(), self.max_request_body_size)
    }
}

impl<S> Service<HttpRequest<HttpBody>> for HttpRequestMiddleware<S>
where
    S: Service<HttpRequest, Response = HttpResponse> + Clone + Send + Unpin + 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = HttpRequestFuture<S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: HttpRequest<HttpBody>) -> Self::Future {
        if !self.check_credentials(request.headers_mut()) {
            let error = ErrorObject::borrowed(401, "unauthenticated method", None);
            return HttpRequestFuture {
                stage: Stage::Rejected(Some(BoxError::from(error))),
            };
        }

        insert_or_replace_content_type_header(request.headers_mut());

        let (parts, body) = request.into_parts();
        HttpRequestFuture {
            stage: Stage::ReadingRequest {
                parts: Some(parts),
                body: Box::pin(Limited::new(body, self.max_request_body_size).collect()),
                service: self.service.clone(),
            },
        }
    }
}

/// The response future of [`HttpRequestMiddleware`], which reads the request, calls the inner service, and reads its response.
pub(crate) struct HttpRequestFuture<S: Service<HttpRequest>> {
    stage: Stage<S>,
}

/// The stage an [`HttpRequestFuture`] has reached.
enum Stage<S: Service<HttpRequest>> {
    /// The request failed authentication before any body was read.
    Rejected(Option<BoxError>),
    /// Reading the request body, before rewriting it and calling the inner service.
    ReadingRequest {
        parts: Option<http::request::Parts>,
        body: Pin<Box<Collect<Limited<HttpBody>>>>,
        service: S,
    },
    /// Awaiting the inner service's response.
    Calling {
        version: JsonRpcVersion,
        response: Pin<Box<S::Future>>,
    },
    /// Reading the response body, before rewriting it in the client's dialect.
    ReadingResponse {
        version: JsonRpcVersion,
        parts: Option<http::response::Parts>,
        body: Pin<Box<Collect<HttpBody>>>,
    },
    /// The future has produced its output.
    Done,
}

impl<S> Future for HttpRequestFuture<S>
where
    S: Service<HttpRequest, Response = HttpResponse> + Unpin,
    S::Error: Into<BoxError>,
{
    type Output = Result<HttpResponse, BoxError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.stage {
                Stage::Rejected(error) => {
                    let error = error
                        .take()
                        .expect("a rejected request yields its error once");
                    self.stage = Stage::Done;
                    return Poll::Ready(Err(error));
                }
                Stage::ReadingRequest {
                    parts,
                    body,
                    service,
                } => {
                    let collected = match body.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            self.stage = Stage::Done;
                            return Poll::Ready(Err(error));
                        }
                        Poll::Ready(Ok(collected)) => collected,
                    };
                    let parts = parts
                        .take()
                        .expect("the request head is taken once, with its body");
                    let (version, request) = request_to_json_rpc_2(parts, collected.to_bytes());
                    let response = Box::pin(service.call(request));
                    self.stage = Stage::Calling { version, response };
                }
                Stage::Calling { version, response } => {
                    let version = *version;
                    let response = match response.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            self.stage = Stage::Done;
                            return Poll::Ready(Err(error.into()));
                        }
                        Poll::Ready(Ok(response)) => response,
                    };
                    let (parts, body) = response.into_parts();
                    self.stage = Stage::ReadingResponse {
                        version,
                        parts: Some(parts),
                        body: Box::pin(body.collect()),
                    };
                }
                Stage::ReadingResponse {
                    version,
                    parts,
                    body,
                } => {
                    let version = *version;
                    let collected = match body.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            self.stage = Stage::Done;
                            return Poll::Ready(Err(error));
                        }
                        Poll::Ready(Ok(collected)) => collected,
                    };
                    let parts = parts
                        .take()
                        .expect("the response head is taken once, with its body");
                    self.stage = Stage::Done;
                    return Poll::Ready(Ok(response_from_json_rpc_2(
                        version,
                        parts,
                        collected.to_bytes(),
                    )));
                }
                Stage::Done => panic!("HttpRequestFuture polled after it completed"),
            }
        }
    }
}

/// The JSON-RPC dialect a client spoke, so its response can be written back in the same dialect.
#[derive(Clone, Copy, Debug)]
enum JsonRpcVersion {
    /// bitcoind's mixture of 1.0, 1.1 and 2.0.
    Bitcoind,
    /// lightwalletd's bitcoind dialect with a nonstandard `"jsonrpc": "1.0"` key.
    Lightwalletd,
    /// Strict JSON-RPC 2.0.
    TwoPointZero,
    /// A body that did not parse, which is passed through for jsonrpsee to reject.
    Unknown,
}

/// A JSON-RPC request in any dialect.
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
    /// The dialect the request is written in.
    fn version(&self) -> JsonRpcVersion {
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
            ) => JsonRpcVersion::TwoPointZero,
            (Some("1.0"), Some(_), Some(_)) => JsonRpcVersion::Lightwalletd,
            (None, Some(_), Some(_)) => JsonRpcVersion::Bitcoind,
            _ => JsonRpcVersion::Unknown,
        }
    }

    /// The same request marked as JSON-RPC 2.0.
    fn into_2(mut self) -> Self {
        self.jsonrpc = Some("2.0".into());
        self
    }
}

/// A JSON-RPC response in any dialect.
#[derive(Debug, Deserialize, Serialize)]
struct JsonRpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<String>,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// The same response written in `version`'s dialect.
    fn into_version(mut self, version: JsonRpcVersion) -> Self {
        match version {
            JsonRpcVersion::Bitcoind => {
                self.jsonrpc = None;
                self.result = self
                    .result
                    .or_else(|| serde_json::value::to_raw_value(&()).ok());
                self.error = self.error.or(Some(serde_json::Value::Null));
            }
            JsonRpcVersion::Lightwalletd => {
                self.jsonrpc = Some("1.0".into());
                self.result = self
                    .result
                    .or_else(|| serde_json::value::to_raw_value(&()).ok());
                self.error = self.error.or(Some(serde_json::Value::Null));
            }
            JsonRpcVersion::TwoPointZero => {
                // A valid `null` result parses as `None`, so it is restored
                // explicitly when there is no error.
                assert_eq!(self.jsonrpc.as_deref(), Some("2.0"));
                if self.error.is_none() {
                    self.result = self
                        .result
                        .or_else(|| serde_json::value::to_raw_value(&()).ok());
                } else {
                    assert!(self.result.is_none());
                }
            }
            JsonRpcVersion::Unknown => (),
        }
        self
    }
}

#[cfg(test)]
mod http_request_middleware {
    use super::*;
    use crate::golden;

    /// A request body larger than this is refused, so the size limit is exercised.
    const MAX_REQUEST_BODY_SIZE: usize = 256;

    /// What a client observes for one request: the status and body, or the transport error.
    #[derive(Debug, PartialEq, Eq, serde::Serialize)]
    struct Outcome {
        name: &'static str,
        status: Option<u16>,
        body: Option<String>,
        error: Option<String>,
    }

    /// One request a client might send.
    struct Case {
        name: &'static str,
        content_type: Option<&'static str>,
        authorization: Option<String>,
        body: String,
    }

    /// An inner service that answers in JSON-RPC 2.0, echoing what reached it so the rewrite is visible.
    fn echo_service() -> impl Service<
        HttpRequest,
        Response = HttpResponse,
        Error = BoxError,
        Future = impl Send + 'static,
    > + Clone
           + Send
           + 'static {
        tower::service_fn(|request: HttpRequest| async move {
            let content_type = request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let bytes = request.into_body().collect().await?.to_bytes();
            let received: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into())
            });
            let id = received
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let reply = match received.get("method").and_then(|method| method.as_str()) {
                Some("null_result") => {
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})
                }
                Some("error") => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -8, "message": "refused"},
                }),
                _ => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"received": received, "contentType": content_type},
                }),
            };
            Ok::<_, BoxError>(HttpResponse::new(HttpBody::from(reply.to_string())))
        })
    }

    /// Every request form the middleware treats differently, authenticated with `secret` where a case needs it.
    fn cases(secret: &str) -> Vec<Case> {
        let credentials = |password: &str| {
            format!(
                "Basic {}",
                STANDARD.encode(format!("__cookie__:{password}"))
            )
        };
        let two_point_zero = r#"{"jsonrpc":"2.0","method":"echo","params":[],"id":1}"#;
        let case = |name, content_type, body: &str| Case {
            name,
            content_type,
            authorization: Some(credentials(secret)),
            body: body.to_string(),
        };
        vec![
            case("two_point_zero", Some("application/json"), two_point_zero),
            case(
                "lightwalletd",
                Some("application/json"),
                r#"{"jsonrpc":"1.0","method":"echo","params":[],"id":"lwd"}"#,
            ),
            case(
                "bitcoind_without_content_type",
                None,
                r#"{"method":"echo","params":[1],"id":7}"#,
            ),
            case(
                "text_plain",
                Some("text/plain; charset=utf-8"),
                two_point_zero,
            ),
            case(
                "form_encoding",
                Some("application/x-www-form-urlencoded"),
                two_point_zero,
            ),
            case("unparseable", Some("application/json"), "not json"),
            case(
                "bitcoind_null_result",
                Some("application/json"),
                r#"{"method":"null_result","params":[],"id":2}"#,
            ),
            case(
                "lightwalletd_error",
                Some("application/json"),
                r#"{"jsonrpc":"1.0","method":"error","params":[],"id":3}"#,
            ),
            case(
                "two_point_zero_null_result",
                Some("application/json"),
                r#"{"jsonrpc":"2.0","method":"null_result","params":[],"id":4}"#,
            ),
            case(
                "oversized_body",
                Some("application/json"),
                &format!(
                    r#"{{"jsonrpc":"2.0","method":"echo","params":["{}"],"id":5}}"#,
                    "a".repeat(MAX_REQUEST_BODY_SIZE)
                ),
            ),
            Case {
                authorization: None,
                ..case("no_credentials", Some("application/json"), two_point_zero)
            },
            Case {
                authorization: Some(credentials("not the cookie")),
                ..case(
                    "wrong_credentials",
                    Some("application/json"),
                    two_point_zero,
                )
            },
        ]
    }

    /// Builds the HTTP request for `case`.
    fn request(case: &Case) -> HttpRequest<HttpBody> {
        let mut builder = http::Request::builder().method("POST").uri("/");
        if let Some(content_type) = case.content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(authorization) = &case.authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        builder
            .body(HttpBody::from(case.body.clone()))
            .expect("every test request is well-formed")
    }

    /// Sends `case` through `middleware` and records what the client observes.
    async fn outcome<M>(case: &Case, middleware: M) -> Outcome
    where
        M: Service<HttpRequest<HttpBody>, Response = HttpResponse, Error = BoxError>,
    {
        match tower::ServiceExt::oneshot(middleware, request(case)).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let bytes = response
                    .into_body()
                    .collect()
                    .await
                    .expect("the echo body is readable")
                    .to_bytes();
                Outcome {
                    name: case.name,
                    status: Some(status),
                    body: Some(String::from_utf8_lossy(&bytes).into()),
                    error: None,
                }
            }
            Err(error) => Outcome {
                name: case.name,
                status: None,
                body: None,
                error: Some(error.to_string()),
            },
        }
    }

    #[tokio::test]
    async fn rewrites_every_request_form_exactly_as_zebra_did() {
        let secret = "a secret the test chose";

        let mut outcomes = Vec::new();
        for case in cases(secret) {
            outcomes.push(
                outcome(
                    &case,
                    HttpRequestMiddleware::new(
                        echo_service(),
                        Some(Cookie::from_secret(secret.to_string())),
                        MAX_REQUEST_BODY_SIZE,
                    ),
                )
                .await,
            );
        }

        golden::assert_golden(
            &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("http"),
            "http_request_middleware",
            &outcomes,
        );
    }
}
