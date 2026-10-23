//! Public health on the gRPC listener: `GET /readyz` + `GET /status`, over HTTP/1.1 or h2
//!
//! - load-balancer monitors speak HTTPS/1.1 (`200` here also proves TLS + this listener up)
//! - answered before admission: never a work permit, never counted as a gRPC request
//! - per connection: [`BURST`] at once, refilled [`PER_SECOND`] (per client ≤
//!   `max_connections_per_ip` × that), over = `429`
//! - anything else over HTTP/1.x = `404` (gRPC needs h2)

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{header, Method, Request, Response, StatusCode, Version};
use http_body_util::{Either, Full};
use tokio::time::Instant;
use tower::ServiceExt as _;

/// What the health paths answer (zainod: its `/readyz` + public `/status` projection)
pub trait Health: Send + Sync + 'static {
    /// `(ready, JSON body)`: `200` when ready, else `503`
    fn readyz(&self) -> (bool, String);
    /// Public JSON, always `200`
    fn status(&self) -> String;
}

const READYZ: &str = "/readyz";
const STATUS: &str = "/status";

/// Health requests one connection may send at once
const BURST: f64 = 10.0;

/// Health requests one connection regains per second
const PER_SECOND: f64 = 1.0;

/// One connection's health allowance
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self) -> bool {
        let now = Instant::now();
        let refilled = self.tokens + (now - self.at).as_secs_f64() * PER_SECOND;
        self.tokens = refilled.min(BURST);
        self.at = now;
        let allowed = self.tokens >= 1.0;
        if allowed {
            self.tokens -= 1.0;
        }
        allowed
    }
}

/// Health paths answered here, the rest passed on with their RPC resolved (outermost layer, one
/// per connection)
#[derive(Clone)]
pub(crate) struct HealthRoute<Inner> {
    inner: Inner,
    health: Option<Arc<dyn Health>>,
    bucket: Arc<Mutex<Bucket>>,
}

impl<Inner> HealthRoute<Inner> {
    pub(crate) fn new(inner: Inner, health: Option<Arc<dyn Health>>) -> Self {
        let bucket = Bucket { tokens: BURST, at: Instant::now() };
        Self { inner, health, bucket: Arc::new(Mutex::new(bucket)) }
    }

    fn answer(&self, path: &str) -> Option<Response<Full<Bytes>>> {
        let health = self.health.as_ref()?;
        let (status, body) = match path {
            READYZ => match health.readyz() {
                (true, body) => (StatusCode::OK, body),
                (false, body) => (StatusCode::SERVICE_UNAVAILABLE, body),
            },
            STATUS => (StatusCode::OK, health.status()),
            _ => return None,
        };
        Some(json(status, body))
    }
}

fn json(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    plain(status, body, "application/json")
}

fn plain(status: StatusCode, body: String, content_type: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

type Answered<Body> = Response<Either<Full<Bytes>, Body>>;

impl<Inner, ReqBody, ResBody> tower::Service<Request<ReqBody>> for HealthRoute<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    Inner::Error: Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = Answered<ResBody>;
    type Error = Inner::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    /// `inner` readied inside the call (`oneshot`)
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        let path = request.uri().path();
        let health_path = matches!(path, READYZ | STATUS) && self.health.is_some();
        let local = if health_path && request.method() == Method::GET {
            let allowed = self.bucket.lock().expect("health bucket poisoned").take();
            match allowed {
                true => self.answer(path),
                false => Some(plain(StatusCode::TOO_MANY_REQUESTS, String::new(), "text/plain")),
            }
        } else if health_path {
            Some(plain(StatusCode::METHOD_NOT_ALLOWED, String::new(), "text/plain"))
        } else if request.version() < Version::HTTP_2 {
            Some(plain(StatusCode::NOT_FOUND, String::new(), "text/plain"))
        } else {
            None
        };
        if let Some(response) = local {
            return Box::pin(std::future::ready(Ok(response.map(Either::Left))));
        }

        crate::service::Method::resolve(&mut request);
        let inner = self.inner.clone();
        Box::pin(async move { Ok(inner.oneshot(request).await?.map(Either::Right)) })
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use http_body_util::BodyExt as _;
    use tower::Service as _;

    use super::*;

    /// Answers every request it is handed with `"passed"` (stands in for the gRPC stack)
    #[derive(Clone)]
    struct Passed;

    impl tower::Service<Request<()>> for Passed {
        type Response = Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<()>) -> Self::Future {
            std::future::ready(Ok(Response::new(Full::new(Bytes::from_static(b"passed")))))
        }
    }

    #[derive(Clone, Copy)]
    struct Fixed {
        ready: bool,
    }

    impl Health for Fixed {
        fn readyz(&self) -> (bool, String) {
            (
                self.ready,
                format!(r#"{{"state":"{}"}}"#, if self.ready { "serving" } else { "syncing" }),
            )
        }
        fn status(&self) -> String {
            r#"{"public":true}"#.to_owned()
        }
    }

    /// `(status, body)` of one request
    async fn send(
        route: &mut HealthRoute<Passed>,
        method: Method,
        path: &str,
        version: Version,
    ) -> (u16, String) {
        let request =
            Request::builder().method(method).uri(path).version(version).body(()).expect("request");
        let response = route.call(request).await.expect("infallible");
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.expect("body").to_bytes();
        (status, String::from_utf8(body.to_vec()).expect("utf-8"))
    }

    /// "passed" = handed on to the gRPC stack
    #[tokio::test]
    async fn health_paths_are_answered_beside_grpc_over_both_http_versions() {
        let (h1, h2) = (Version::HTTP_11, Version::HTTP_2);
        let (serving, syncing) = (Some(Fixed { ready: true }), Some(Fixed { ready: false }));
        let grpc = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";

        #[rustfmt::skip]
        let cases = [
            ("ready /readyz",            serving, Method::GET,  READYZ, h1, 200, r#"{"state":"serving"}"#),
            ("not ready /readyz",        syncing, Method::GET,  READYZ, h1, 503, r#"{"state":"syncing"}"#),
            ("/status over h2",          serving, Method::GET,  STATUS, h2, 200, r#"{"public":true}"#),
            ("POST to a health path",    serving, Method::POST, READYZ, h1, 405, ""),
            ("gRPC path over HTTP/1.1",  serving, Method::GET,  grpc,   h1, 404, ""),
            ("gRPC path over h2",        serving, Method::POST, grpc,   h2, 200, "passed"),
            ("no Health wired, h2",      None,    Method::GET,  READYZ, h2, 200, "passed"),
            ("no Health wired, HTTP/1.1", None,   Method::GET,  READYZ, h1, 404, ""),
        ];
        for (case, health, method, path, version, status, body) in cases {
            let health = health.map(|fixed| Arc::new(fixed) as Arc<dyn Health>);
            let mut route = HealthRoute::new(Passed, health);

            let answered = send(&mut route, method, path, version).await;

            assert_eq!(answered, (status, body.to_owned()), "{case}: {path} {version:?}");
        }
    }

    /// Paused clock: `BURST` at once, then one back per `PER_SECOND`
    #[tokio::test(start_paused = true)]
    async fn health_requests_are_rate_limited_per_connection() {
        let mut route = HealthRoute::new(Passed, Some(Arc::new(Fixed { ready: true }) as _));
        let mut readyz = async || send(&mut route, Method::GET, READYZ, Version::HTTP_11).await.0;
        let burst = BURST as usize;

        let mut codes = Vec::new();
        for _ in 0..=burst {
            codes.push(readyz().await);
        }
        tokio::time::advance(Duration::from_secs_f64(1.0 / PER_SECOND)).await;
        let refilled = [readyz().await, readyz().await];

        assert_eq!(codes, [vec![200; burst], vec![429]].concat(), "{burst} at once, then 429");
        assert_eq!(refilled, [200, 429], "one back a second later");
    }
}
