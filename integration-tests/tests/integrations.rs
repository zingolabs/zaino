//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zingo_rpc::walletrpc::grpc::GrpcConnector;
use zingoproxy_testutils::{drop_test_manager, get_proxy_uri, launch_test_manager};

mod proxy {
    use super::*;

    #[tokio::test]
    async fn connect_to_lwd_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (conf_path, _regtest_manager, regtest_handles, _handles, proxy_port, _nym_addr) =
            launch_test_manager(online.clone()).await;

        let proxy_uri = get_proxy_uri(proxy_port);
        println!("Attempting to connect to GRPC server at URI: {}", proxy_uri);

        let mut client = GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .expect("Failed to create GRPC client");

        let lightd_info = client
            .get_lightd_info(zcash_client_backend::proto::service::Empty {})
            .await
            .expect("Failed to retrieve lightd info from GRPC server");

        println!("{:#?}", lightd_info.into_inner());

        drop_test_manager(Some(conf_path), regtest_handles, online).await
    }

    #[tokio::test]
    async fn send_over_proxy() {
        let online = Arc::new(AtomicBool::new(true));
        let (conf_path, _regtest_manager, regtest_handles, _handles, proxy_port, _nym_addr) =
            launch_test_manager(online.clone()).await;

        let proxy_uri = get_proxy_uri(proxy_port);
        println!("Attempting to connect to GRPC server at URI: {}", proxy_uri);

        let mut client = GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .expect("Failed to create GRPC client");

        drop_test_manager(Some(conf_path), regtest_handles, online).await
    }
}

mod nym {
    // TODO: Add Nym Tests.
}

mod darkside {
    // TODO: Add darkside tests.
}
