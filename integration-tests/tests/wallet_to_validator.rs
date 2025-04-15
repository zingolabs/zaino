//! Holds wallet-to-validator tests for Zaino.

#![forbid(unsafe_code)]

use std::sync::Arc;
use zaino_testutils::from_inputs;
use zaino_testutils::TestManager;
use zaino_testutils::Validator as _;

mod wallet_basic {
    use zaino_fetch::jsonrpsee::connector::test_node_and_return_url;
    use zaino_testutils::ValidatorKind;

    use super::*;

    #[tokio::test]
    async fn zcashd_connect_to_node_get_info() {
        connect_to_node_get_info(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn zebrad_connect_to_node_get_info() {
        connect_to_node_get_info(&ValidatorKind::Zebrad).await;
    }

    async fn connect_to_node_get_info(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_info().await;
        clients.recipient.do_info().await;

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_send_to_orchard() {
        send_to_orchard(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn zebrad_send_to_orchard() {
        send_to_orchard(&ValidatorKind::Zebrad).await;
    }

    async fn send_to_orchard(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .orchard_balance
                .unwrap(),
            250_000
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_send_to_sapling() {
        send_to_sapling(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn zebrad_send_to_sapling() {
        send_to_sapling(&ValidatorKind::Zebrad).await;
    }

    async fn send_to_sapling(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("sapling").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .sapling_balance
                .unwrap(),
            250_000
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_send_to_transparent() {
        send_to_transparent(&ValidatorKind::Zcashd).await;
    }

    /// Bug documented in https://github.com/zingolabs/zaino/issues/145.
    #[tokio::test]
    async fn zebrad_send_to_transparent() {
        send_to_transparent(&ValidatorKind::Zebrad).await;
    }

    async fn send_to_transparent(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let fetch_service =
            zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector::new_with_basic_auth(
                test_node_and_return_url(
                    test_manager.zebrad_rpc_listen_address,
                    false,
                    None,
                    Some("xxxxxx".to_string()),
                    Some("xxxxxx".to_string()),
                )
                .await
                .unwrap(),
                "xxxxxx".to_string(),
                "xxxxxx".to_string(),
            )
            .unwrap();

        println!("\n\nFetching Chain Height!\n");

        let height = dbg!(fetch_service.get_blockchain_info().await.unwrap().blocks.0);

        println!("\n\nFetching Tx From Unfinalized Chain!\n");

        let unfinalised_transactions = fetch_service
            .get_address_txids(
                vec![clients.get_recipient_address("transparent").await],
                height,
                height,
            )
            .await
            .unwrap();

        dbg!(unfinalised_transactions.clone());

        // Generate blocks
        //
        // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
        //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
        for height in 1..=99 {
            dbg!("Generating block at height: {}", height);
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        println!("\n\nFetching Tx From Finalized Chain!\n");

        let finalised_transactions = fetch_service
            .get_address_txids(
                vec![clients.get_recipient_address("transparent").await],
                height,
                height,
            )
            .await
            .unwrap();

        dbg!(finalised_transactions.clone());

        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .transparent_balance
                .unwrap(),
            250_000
        );

        assert_eq!(unfinalised_transactions, finalised_transactions);
        // test_manager.local_net.print_stdout();

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_send_to_all() {
        send_to_all(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn zebrad_send_to_all() {
        send_to_all(&ValidatorKind::Zebrad).await;
    }

    async fn send_to_all(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        test_manager.local_net.generate_blocks(2).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();

        // "Create" 3 orchard notes in faucet.
        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("sapling").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        // Generate blocks
        //
        // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
        //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
        for height in 1..=100 {
            dbg!("Generating block at height: {}", height);
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .orchard_balance
                .unwrap(),
            250_000
        );
        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .sapling_balance
                .unwrap(),
            250_000
        );
        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .transparent_balance
                .unwrap(),
            250_000
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_shield() {
        shield(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn zebrad_shield() {
        shield(&ValidatorKind::Zebrad).await;
    }

    async fn shield(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        // Generate blocks
        //
        // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
        //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
        for height in 1..=100 {
            dbg!("Generating block at height: {}", height);
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .transparent_balance
                .unwrap(),
            250_000
        );

        clients.recipient.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();

        assert_eq!(
            clients
                .recipient
                .do_balance()
                .await
                .orchard_balance
                .unwrap(),
            235_000
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn zcashd_monitor_unverified_mempool() {
        monitor_unverified_mempool(&ValidatorKind::Zcashd).await;
    }

    /// Bug documented in https://github.com/zingolabs/zaino/issues/144.
    #[tokio::test]
    async fn zebrad_monitor_unverified_mempool() {
        monitor_unverified_mempool(&ValidatorKind::Zebrad).await;
    }

    async fn monitor_unverified_mempool(validator: &ValidatorKind) {
        let mut test_manager = TestManager::launch(validator, None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .take()
            .expect("Clients are not initialized");
        let recipient_client = Arc::new(clients.recipient);

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();

        if matches!(validator, ValidatorKind::Zebrad) {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let txid_1 = from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &zaino_testutils::get_base_address_macro!(recipient_client, "unified"),
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        let txid_2 = from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &zaino_testutils::get_base_address_macro!(recipient_client, "sapling"),
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        println!("\n\nStarting Mempool!\n");

        recipient_client.clear_state().await;
        zaino_testutils::LightClient::start_mempool_monitor(recipient_client.clone()).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // test_manager.local_net.print_stdout();

        let fetch_service =
            zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector::new_with_basic_auth(
                test_node_and_return_url(
                    test_manager.zebrad_rpc_listen_address,
                    false,
                    None,
                    Some("xxxxxx".to_string()),
                    Some("xxxxxx".to_string()),
                )
                .await
                .unwrap(),
                "xxxxxx".to_string(),
                "xxxxxx".to_string(),
            )
            .unwrap();

        println!("\n\nFetching Raw Mempool!\n");

        let mempool_txids = fetch_service.get_raw_mempool().await.unwrap();
        dbg!(txid_1);
        dbg!(txid_2);
        dbg!(mempool_txids.clone());

        println!("\n\nFetching Mempool Tx 1!\n");
        let _transaction_1 = dbg!(
            fetch_service
                .get_raw_transaction(mempool_txids.transactions[0].clone(), Some(1))
                .await
        );

        println!("\n\nFetching Mempool Tx 2!\n");
        let _transaction_2 = dbg!(
            fetch_service
                .get_raw_transaction(mempool_txids.transactions[1].clone(), Some(1))
                .await
        );

        assert_eq!(
            recipient_client
                .do_balance()
                .await
                .unverified_orchard_balance
                .unwrap(),
            250_000
        );
        assert_eq!(
            recipient_client
                .do_balance()
                .await
                .unverified_sapling_balance
                .unwrap(),
            250_000
        );

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        println!("\n\nFetching Mined Tx 1!\n");
        let _transaction_1 = dbg!(
            fetch_service
                .get_raw_transaction(mempool_txids.transactions[0].clone(), Some(1))
                .await
        );

        println!("\n\nFetching Mined Tx 2!\n");
        let _transaction_2 = dbg!(
            fetch_service
                .get_raw_transaction(mempool_txids.transactions[1].clone(), Some(1))
                .await
        );

        recipient_client.do_sync(true).await.unwrap();

        assert_eq!(
            recipient_client
                .do_balance()
                .await
                .verified_orchard_balance
                .unwrap(),
            250_000
        );
        assert_eq!(
            recipient_client
                .do_balance()
                .await
                .verified_sapling_balance
                .unwrap(),
            250_000
        );

        test_manager.close().await;
    }
}
