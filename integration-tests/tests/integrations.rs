//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zingo_rpc::walletrpc::grpc::GrpcConnector;
use zingoproxy_testutils::{drop_test_manager, get_proxy_uri, TestManager};

mod proxy {
    use super::*;

    #[tokio::test]
    async fn connect_to_lwd_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;

        let proxy_uri = get_proxy_uri(test_manager.proxy_port);
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

        drop_test_manager(regtest_handler, online).await;
    }

    #[tokio::test]
    async fn send_over_proxy() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;

        let proxy_uri = get_proxy_uri(test_manager.proxy_port);
        let mut client = GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .expect("Failed to create GRPC client");

        drop_test_manager(regtest_handler, online).await;
    }
}

mod nym {
    // TODO: Add Nym Tests.
}

mod darkside {
    // TODO: Add darkside tests.
}
