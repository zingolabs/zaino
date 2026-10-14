//! Public health on the gRPC listener: `GET /readyz` + `GET /status`, over HTTP/1.1 or h2
//!
//! - load-balancer monitors speak HTTPS/1.1; a `200` here also proves TLS + this listener up
//! - answered before admission: never a work permit, never counted as a gRPC request
//! - per connection: [`BURST`] at once, refilled [`PER_SECOND`] (per client ≤
//!   `max_connections_per_ip` × that); over = `429`
//! - anything else over HTTP/1.x = `404` (gRPC needs h2)

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{header, Method, Request, Response, StatusCode, Version};
use http_body_util::{Either, Full};
use tokio::time::Instant;

use crate::service::take_ready;

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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
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
        let mut inner = take_ready(&mut self.inner);
        Box::pin(async move { Ok(inner.call(request).await?.map(Either::Right)) })
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

    /// Paused clock:
    /// - `/readyz` = 200 serving / 503 not, `/status` = 200, both over HTTP/1.1 and h2
    /// - a non-GET health request = 405; any other path over HTTP/1.1 = 404, over h2 = passed on
    /// - no `Health` wired = every path passed on (h2) or 404 (HTTP/1.1)
    /// - 10 at once, the 11th = 429; one more per second after that
    #[tokio::test(start_paused = true)]
    async fn health_is_answered_beside_grpc_and_rate_limited_per_connection() {
        let h1 = Version::HTTP_11;
        let h2 = Version::HTTP_2;
        let grpc = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";
        let mut serving = HealthRoute::new(Passed, Some(Arc::new(Fixed { ready: true }) as _));
        let cases = [
            (Method::GET, READYZ, h1, (200, r#"{"state":"serving"}"#)),
            (Method::GET, STATUS, h2, (200, r#"{"public":true}"#)),
            (Method::POST, READYZ, h1, (405, "")),
            (Method::GET, grpc, h1, (404, "")),
            (Method::POST, grpc, h2, (200, "passed")),
        ];
        for (method, path, version, (status, body)) in cases {
            let got = send(&mut serving, method, path, version).await;
            assert_eq!(got, (status, body.to_owned()), "{path} {version:?}");
        }

        let mut syncing = HealthRoute::new(Passed, Some(Arc::new(Fixed { ready: false }) as _));
        let got = send(&mut syncing, Method::GET, READYZ, h1).await;
        assert_eq!(got, (503, r#"{"state":"syncing"}"#.to_owned()), "not ready = 503");

        let mut bare = HealthRoute::new(Passed, None);
        assert_eq!(send(&mut bare, Method::GET, READYZ, h2).await.1, "passed", "no Health wired");
        assert_eq!(send(&mut bare, Method::GET, READYZ, h1).await.0, 404);

        let mut burst = HealthRoute::new(Passed, Some(Arc::new(Fixed { ready: true }) as _));
        let mut codes = Vec::new();
        for _ in 0..11 {
            codes.push(send(&mut burst, Method::GET, READYZ, h1).await.0);
        }
        assert_eq!(codes, [[200; 10].as_slice(), &[429]].concat(), "10 at once, then 429");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(send(&mut burst, Method::GET, READYZ, h1).await.0, 200, "one back per second");
        assert_eq!(send(&mut burst, Method::GET, READYZ, h1).await.0, 429);
    }
}
