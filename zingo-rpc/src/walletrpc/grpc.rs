//! A Nym enabled GrpcConnector for cunsumption by zcash wallets and integration tests.

use std::sync::Arc;
use tower::ServiceExt;

use http::Uri;
use http_body::combinators::UnsyncBoxBody;
use hyper::client::HttpConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tonic::Status;
use tower::util::BoxCloneService;

use crate::walletrpc::service::NymTxStreamerClient;
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

type UnderlyingService = BoxCloneService<
    http::Request<UnsyncBoxBody<prost::bytes::Bytes, Status>>,
    http::Response<hyper::Body>,
    hyper::Error,
>;

/// GrpcConnector Config.
#[derive(Clone)]
pub struct GrpcConnector {
    uri: http::Uri,
}

impl GrpcConnector {
    /// Creates a new GrpcConnector.
    pub fn new(uri: http::Uri) -> Self {
        Self { uri }
    }

    /// Returns GrpcConnector listen uri.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Returns NymTxStreamerClient.
    pub fn get_client(
        &self,
    ) -> impl std::future::Future<
        Output = Result<NymTxStreamerClient<UnderlyingService>, Box<dyn std::error::Error>>,
    > {
        let uri = Arc::new(self.uri.clone());
        async move {
            let mut http_connector = HttpConnector::new();
            http_connector.enforce_http(false);
            if uri.scheme_str() == Some("https") {
                let mut roots = RootCertStore::empty();
                roots.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(
                    |anchor_ref| {
                        tokio_rustls::rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                            anchor_ref.subject,
                            anchor_ref.spki,
                            anchor_ref.name_constraints,
                        )
                    },
                ));

                #[cfg(test)]
                add_test_cert_to_roots(&mut roots);

                let tls = ClientConfig::builder()
                    .with_safe_defaults()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let connector = tower::ServiceBuilder::new()
                    .layer_fn(move |s| {
                        let tls = tls.clone();

                        hyper_rustls::HttpsConnectorBuilder::new()
                            .with_tls_config(tls)
                            .https_or_http()
                            .enable_http2()
                            .wrap_connector(s)
                    })
                    .service(http_connector);
                let client = Box::new(hyper::Client::builder().build(connector));
                let uri = uri.clone();
                let svc = tower::ServiceBuilder::new()
                    //Here, we take all the pieces of our uri, and add in the path from the Requests's uri
                    .map_request(move |mut req: http::Request<tonic::body::BoxBody>| {
                        let uri = Uri::builder()
                            .scheme(uri.scheme().unwrap().clone())
                            .authority(uri.authority().unwrap().clone())
                            //here. The Request's uri contains the path to the GRPC sever and
                            //the method being called
                            .path_and_query(req.uri().path_and_query().unwrap().clone())
                            .build()
                            .unwrap();

                        *req.uri_mut() = uri;
                        req
                    })
                    .service(client);

                Ok(NymTxStreamerClient {
                    compact_tx_streamer_client: CompactTxStreamerClient::new(svc.boxed_clone()),
                })
            } else {
                let connector = tower::ServiceBuilder::new().service(http_connector);
                let client = Box::new(hyper::Client::builder().http2_only(true).build(connector));
                let uri = uri.clone();
                let svc = tower::ServiceBuilder::new()
                    //Here, we take all the pieces of our uri, and add in the path from the Requests's uri
                    .map_request(move |mut req: http::Request<tonic::body::BoxBody>| {
                        let uri = Uri::builder()
                            .scheme(uri.scheme().unwrap().clone())
                            .authority(uri.authority().unwrap().clone())
                            //here. The Request's uri contains the path to the GRPC sever and
                            //the method being called
                            .path_and_query(req.uri().path_and_query().unwrap().clone())
                            .build()
                            .unwrap();

                        *req.uri_mut() = uri;
                        req
                    })
                    .service(client);

                // Ok(CompactTxStreamerClient::new(svc.boxed_clone()))
                Ok(NymTxStreamerClient {
                    compact_tx_streamer_client: CompactTxStreamerClient::new(svc.boxed_clone()),
                })
            }
        }
    }
}

#[cfg(test)]
fn add_test_cert_to_roots(roots: &mut RootCertStore) {
    const TEST_PEMFILE_PATH: &str = "test-data/localhost.pem";
    let fd = std::fs::File::open(TEST_PEMFILE_PATH).unwrap();
    let mut buf = std::io::BufReader::new(&fd);
    let certs = rustls_pemfile::certs(&mut buf).unwrap();
    roots.add_parsable_certificates(&certs);
}
