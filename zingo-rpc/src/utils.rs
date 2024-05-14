//! Utility functions for Zingo-RPC.

use std::sync::Arc;
use tower::ServiceExt;

use http::Uri;
use http_body::combinators::UnsyncBoxBody;
use hyper::client::HttpConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tonic::Status;
use tower::util::BoxCloneService;

use crate::proto::darkside::darkside_streamer_client::DarksideStreamerClient;
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

/// Passes unimplemented RPCs on to Lightwalletd.
#[macro_export]
macro_rules! define_grpc_passthrough {
    (fn
        $name:ident(
            &$self:ident$(,$($arg:ident: $argty:ty,)*)?
        ) -> $ret:ty
    ) => {
        #[must_use]
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn $name<'life0, 'async_trait>(&'life0 $self$($(, $arg: $argty)*)?) ->
           ::core::pin::Pin<Box<
                dyn ::core::future::Future<
                    Output = ::core::result::Result<
                        ::tonic::Response<$ret>,
                        ::tonic::Status
                >
            > + ::core::marker::Send + 'async_trait
        >>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            println!("received call of {}", stringify!($name));
                Box::pin(async {
                    GrpcConnector::new($self.lightwalletd_uri.clone())
                        .get_client()
                        .await
                        .expect("Proxy server failed to create client")
                        .$name($($($arg),*)?)
                        .await
                })

        }
    };
}

type UnderlyingService = BoxCloneService<
    http::Request<UnsyncBoxBody<prost::bytes::Bytes, Status>>,
    http::Response<hyper::Body>,
    hyper::Error,
>;

#[derive(Clone)]
pub struct GrpcConnector {
    uri: http::Uri,
}

impl GrpcConnector {
    pub fn new(uri: http::Uri) -> Self {
        Self { uri }
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn get_client(
        &self,
    ) -> impl std::future::Future<
        Output = Result<CompactTxStreamerClient<UnderlyingService>, Box<dyn std::error::Error>>,
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

                Ok(CompactTxStreamerClient::new(svc.boxed_clone()))
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

                Ok(CompactTxStreamerClient::new(svc.boxed_clone()))
            }
        }
    }

    pub fn get_darkside_client(
        &self,
    ) -> impl std::future::Future<
        Output = Result<DarksideStreamerClient<UnderlyingService>, Box<dyn std::error::Error>>,
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

                Ok(DarksideStreamerClient::new(svc.boxed_clone()))
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

                Ok(DarksideStreamerClient::new(svc.boxed_clone()))
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
