//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zingoindexer_testutils::{
    drop_test_manager, get_zingo_address, start_zingo_mempool_monitor, ProxyPool, TestManager,
};

mod wallet_basic {
    use super::*;

    #[tokio::test]
    async fn connect_to_node_get_info() {
        let online = Arc::new(AtomicBool::new(true));
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        let lightd_info = zingo_client.do_info().await;
        println!(
            "@zingoindexertest: Lightd_info response:\n{:#?}.",
            lightd_info
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "unified").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "unified").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(&[ProxyPool::Sapling.into()], None)
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(&[ProxyPool::Transparent.into()], None)
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.sapling_balance.unwrap(), 250_000);
        assert_eq!(balance.transparent_balance.unwrap(), 250_000);

        zingo_client
            .do_shield(
                &[ProxyPool::Sapling.into(), ProxyPool::Transparent.into()],
                None,
            )
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(2).unwrap();
        zingo_client.do_sync(false).await.unwrap();

        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "unified").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "transparent").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        test_manager.regtest_manager.generate_n_blocks(30).unwrap();

        println!("@zingoindexertest: syncing full batch.");
        zingo_client.do_sync(false).await.unwrap();

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
        let (test_manager, regtest_handler, _indexer_handler) =
            TestManager::launch(online.clone()).await;
        let zingo_client = test_manager.build_lightclient().await;

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();
        zingo_client
            .do_send(vec![(
                &get_zingo_address(&zingo_client, "sapling").await,
                250_000,
                None,
            )])
            .await
            .unwrap();

        start_zingo_mempool_monitor(&zingo_client).await;

        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
        assert_eq!(balance.unverified_sapling_balance.unwrap(), 500_000);

        test_manager.regtest_manager.generate_n_blocks(1).unwrap();
        zingo_client.do_sync(false).await.unwrap();
        let balance = zingo_client.do_balance().await;
        println!("@zingoindexertest: zingo_client balance: \n{:#?}.", balance);
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
