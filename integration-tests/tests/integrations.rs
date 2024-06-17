//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zingo_netutils::GrpcConnector;
use zingolib::lightclient::LightClient;
use zingoproxy_testutils::{drop_test_manager, TestManager};

mod wallet_basic {
    use super::*;

    #[tokio::test]
    async fn connect_to_node_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;

        println!(
            "@zingoproxytest: Attempting to connect to GRPC server at URI: {}.",
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

        println!(
            "@zingoproxytest: Lightd_info response:\n{:#?}.",
            lightd_info.into_inner()
        );
        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn send_to_orchard() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "unified"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.orchard_balance.unwrap(), 1_875_000_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn send_to_sapling() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn send_to_transparent() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "transparent"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn send_to_multiple() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(2).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "unified"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "transparent"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.orchard_balance.unwrap(), 2_499_500_000);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn shield_from_sapling() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(
                &[
                    zingolib::wallet::Pool::Sapling,
                    // zingolib::wallet::Pool::Transparent,
                ],
                None,
            )
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 0);
        assert_eq!(balance.orchard_balance.unwrap(), 2_500_000_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn shield_from_transparent() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "transparent"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(
                &[
                    // zingolib::wallet::Pool::Sapling,
                    zingolib::wallet::Pool::Transparent,
                ],
                None,
            )
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.transparent_balance.unwrap(), 0);
        assert_eq!(balance.orchard_balance.unwrap(), 2_500_000_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn shield_from_multiple() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "transparent"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(
                &[
                    zingolib::wallet::Pool::Sapling,
                    zingolib::wallet::Pool::Transparent,
                ],
                None,
            )
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 0);
        assert_eq!(balance.transparent_balance.unwrap(), 0);
        assert_eq!(balance.orchard_balance.unwrap(), 2_500_000_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn sync_full_batch() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(2).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "unified"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "transparent"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.orchard_balance.unwrap(), 76_874_500_000);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }

    #[tokio::test]
    async fn monitor_unverified_mempool() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _proxy_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &zingolib::get_base_address!(zingo_client, "sapling"),
                250_000,
                None,
            )])
            .await
            .unwrap();

        let zingo_client_saved = zingo_client.export_save_buffer_async().await.unwrap();
        let zingo_client_loaded = std::sync::Arc::new(
            LightClient::read_wallet_from_buffer_async(
                zingo_client.config(),
                &zingo_client_saved[..],
            )
            .await
            .unwrap(),
        );
        LightClient::start_mempool_monitor(zingo_client_loaded.clone());
        // This seems to be long enough for the mempool monitor to kick in.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.unverified_sapling_balance.unwrap(), 500_000);

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        let balance = zingo_client.do_balance().await;
        println!("@zingoproxytest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.verified_sapling_balance.unwrap(), 500_000);

        drop_test_manager(
            Some(test_manager.temp_conf_dir.path().to_path_buf()),
            regtest_handler,
            online,
        )
        .await;
    }
}

mod nym {
    // TODO: Build nym enhanced zingolib version using zingo-rpc::walletrpc::service.
}

mod darkside {
    // TODO: Add darkside.
}
