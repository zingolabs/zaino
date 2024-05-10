//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
// use zingo_rpc::walletrpc::grpc::GrpcConnector;
use zingoproxy_testutils::{drop_test_manager, TestManager};

mod proxy {
    use zingo_netutils::GrpcConnector;

    use super::*;

    #[tokio::test]
    async fn connect_to_lwd_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;

        println!(
            "Attempting to connect to GRPC server at URI: {}",
            test_manager.get_proxy_uri()
        );

        let mut client = GrpcConnector::new(test_manager.get_proxy_uri())
            .get_client()
            .await
            .expect("Failed to create GRPC client");

        let lightd_info = client
            .get_lightd_info(zcash_client_backend::proto::service::Empty {})
            .await
            .expect("Failed to retrieve lightd info from GRPC server");

        println!("{:#?}", lightd_info.into_inner());

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn send_over_proxy() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        println!(
            "zingo_client balance: {:#?}",
            zingo_client.do_balance().await
        );

        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client.do_sync(false).await.unwrap();

        println!(
            "zingo_client balance: {:#?}",
            zingo_client.do_balance().await
        );

        assert_eq!(
            zingo_client.do_balance().await.sapling_balance.unwrap(),
            250_000
        );

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }
}

mod nym {
    // TODO: Add Nym Tests.
}

mod darkside {
    // TODO: Add darkside tests.
}
