//! Integration tests for zingo-Indexer.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zaino_testutils::{
    drop_test_manager,
    zingo_lightclient::{get_address, start_mempool_monitor},
    TestManager,
};

mod wallet_basic {
    use zingolib::testutils::lightclient::from_inputs;

    use super::*;

    #[tokio::test]
    async fn connect_to_node_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        let lightd_info = zingo_client.do_info().await;
        println!("[TEST LOG] Lightd_info response:\n{:#?}.", lightd_info);

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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "unified").await, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "sapling").await, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(
                &get_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(2).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "unified").await, 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "sapling").await, 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(
                &get_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
    async fn shield_from_transparent() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(
                &get_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        zingo_client.quick_shield().await.unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(2).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        test_manager.regtest_manager.generate_n_blocks(5).unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "unified").await, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(15).unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "sapling").await, 250_000, None)],
        )
        .await
        .unwrap();

        test_manager.regtest_manager.generate_n_blocks(15).unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(
                &get_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.regtest_manager.generate_n_blocks(70).unwrap();

        println!("[TEST LOG] syncing full batch.");
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.orchard_balance.unwrap(), 67_499_500_000);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "sapling").await, 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &zingo_client,
            vec![(&get_address(&zingo_client, "sapling").await, 250_000, None)],
        )
        .await
        .unwrap();

        start_mempool_monitor(&zingo_client).await;

        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.unverified_sapling_balance.unwrap(), 500_000);

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        let balance = zingo_client.do_balance().await;
        println!("[TEST LOG] zingo_client balance: \n{:#?}.", balance);
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
