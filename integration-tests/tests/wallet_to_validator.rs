//! Holds wallet-to-validator tests for Zaino.

#![forbid(unsafe_code)]

use zaino_fetch::jsonrpsee::connector::test_node_and_return_url;
use zaino_state::BackendType;
use zaino_state::LightWalletService;
use zaino_state::ZcashIndexer;
use zaino_state::ZcashService;
use zaino_testutils::from_inputs;
use zaino_testutils::TestManager;
use zaino_testutils::ValidatorKind;
use zainodlib::config::ZainodConfig;
use zainodlib::error::IndexerError;
use zip32::AccountId;

async fn connect_to_node_get_info_for_validator<Service>(
    validator: &ValidatorKind,
    backend: &BackendType,
) where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.do_info().await;
    clients.recipient.do_info().await;

    test_manager.close().await;
}

async fn send_to_orchard<Service>(validator: &ValidatorKind, backend: &BackendType)
where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();
    test_manager.generate_blocks_and_poll(1).await;
    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_orchard_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    test_manager.close().await;
}

async fn send_to_sapling<Service>(validator: &ValidatorKind, backend: &BackendType)
where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_zaddr = clients.get_recipient_address("sapling").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_zaddr, 250_000, None)])
        .await
        .unwrap();
    test_manager.generate_blocks_and_poll(1).await;
    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_sapling_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    test_manager.close().await;
}

async fn send_to_transparent<Service>(validator: &ValidatorKind, backend: &BackendType)
where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();

    test_manager.generate_blocks_and_poll(1).await;

    let fetch_service = zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(
            test_manager.full_node_rpc_listen_address,
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
    test_manager.generate_blocks_and_poll(99).await;

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

    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .confirmed_transparent_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    assert_eq!(unfinalised_transactions, finalised_transactions);
    // test_manager.local_net.print_stdout();

    test_manager.close().await;
}

async fn send_to_all<Service>(validator: &ValidatorKind, backend: &BackendType)
where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    test_manager.generate_blocks_and_poll(2).await;
    clients.faucet.sync_and_await().await.unwrap();

    // "Create" 3 orchard notes in faucet.
    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_zaddr = clients.get_recipient_address("sapling").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_zaddr, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();
    test_manager.generate_blocks_and_poll(100).await;
    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_orchard_balance
            .unwrap()
            .into_u64(),
        250_000
    );
    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_sapling_balance
            .unwrap()
            .into_u64(),
        250_000
    );
    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .confirmed_transparent_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    test_manager.close().await;
}

async fn shield_for_validator<Service>(validator: &ValidatorKind, backend: &BackendType)
where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();
    test_manager.generate_blocks_and_poll(100).await;
    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .confirmed_transparent_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    clients
        .recipient
        .quick_shield(AccountId::ZERO)
        .await
        .unwrap();
    test_manager.generate_blocks_and_poll(1).await;
    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_orchard_balance
            .unwrap()
            .into_u64(),
        235_000
    );

    test_manager.close().await;
}

async fn monitor_unverified_mempool_for_validator<Service>(
    validator: &ValidatorKind,
    backend: &BackendType,
) where
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    let mut test_manager =
        TestManager::<Service>::launch(validator, backend, None, None, None, true, false, true)
            .await
            .unwrap();
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    test_manager.generate_blocks_and_poll(1).await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.generate_blocks_and_poll(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let txid_1 = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(
            &zaino_testutils::get_base_address_macro!(&mut clients.recipient, "unified"),
            250_000,
            None,
        )],
    )
    .await
    .unwrap();
    let txid_2 = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(
            &zaino_testutils::get_base_address_macro!(&mut clients.recipient, "sapling"),
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    println!("\n\nStarting Mempool!\n");
    clients.recipient.wallet.write().await.clear_all();
    clients.recipient.sync_and_await().await.unwrap();

    // test_manager.local_net.print_stdout();

    let fetch_service = zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(
            test_manager.full_node_rpc_listen_address,
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
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .unconfirmed_orchard_balance
            .unwrap()
            .into_u64(),
        250_000
    );
    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .unconfirmed_sapling_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    test_manager.generate_blocks_and_poll(1).await;

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

    clients.recipient.sync_and_await().await.unwrap();

    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .confirmed_orchard_balance
            .unwrap()
            .into_u64(),
        250_000
    );
    assert_eq!(
        clients
            .recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .confirmed_orchard_balance
            .unwrap()
            .into_u64(),
        250_000
    );

    test_manager.close().await;
}

mod zcashd {
    #[allow(deprecated)]
    use zaino_state::FetchService;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    #[allow(deprecated)]
    async fn connect_to_node_get_info() {
        connect_to_node_get_info_for_validator::<FetchService>(
            &ValidatorKind::Zcashd,
            &BackendType::Fetch,
        )
        .await;
    }

    mod sent_to {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn orchard() {
            send_to_orchard::<FetchService>(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn sapling() {
            send_to_sapling::<FetchService>(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn transparent() {
            send_to_transparent::<FetchService>(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn all() {
            send_to_all::<FetchService>(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(deprecated)]
    async fn shield() {
        shield_for_validator::<FetchService>(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(deprecated)]
    async fn monitor_unverified_mempool() {
        monitor_unverified_mempool_for_validator::<FetchService>(
            &ValidatorKind::Zcashd,
            &BackendType::Fetch,
        )
        .await;
    }
}

mod zebrad {
    use super::*;

    mod fetch_service {
        use super::*;
        #[allow(deprecated)]
        use zaino_state::FetchService;

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn connect_to_node_get_info() {
            connect_to_node_get_info_for_validator::<FetchService>(
                &ValidatorKind::Zebrad,
                &BackendType::Fetch,
            )
            .await;
        }
        mod send_to {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn sapling() {
                send_to_sapling::<FetchService>(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn orchard() {
                send_to_orchard::<FetchService>(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }

            /// Bug documented in https://github.com/zingolabs/zaino/issues/145.
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn transparent() {
                send_to_transparent::<FetchService>(&ValidatorKind::Zebrad, &BackendType::Fetch)
                    .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn all() {
                send_to_all::<FetchService>(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }
        }
        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn shield() {
            shield_for_validator::<FetchService>(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
        }
        /// Bug documented in https://github.com/zingolabs/zaino/issues/144.
        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn monitor_unverified_mempool() {
            monitor_unverified_mempool_for_validator::<FetchService>(
                &ValidatorKind::Zebrad,
                &BackendType::Fetch,
            )
            .await;
        }
    }

    mod state_service {
        use super::*;
        #[allow(deprecated)]
        use zaino_state::StateService;

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn connect_to_node_get_info() {
            connect_to_node_get_info_for_validator::<StateService>(
                &ValidatorKind::Zebrad,
                &BackendType::State,
            )
            .await;
        }
        mod send_to {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn sapling() {
                send_to_sapling::<StateService>(&ValidatorKind::Zebrad, &BackendType::State).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn orchard() {
                send_to_orchard::<StateService>(&ValidatorKind::Zebrad, &BackendType::State).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn transparent() {
                send_to_transparent::<StateService>(&ValidatorKind::Zebrad, &BackendType::State)
                    .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn all() {
                send_to_all::<StateService>(&ValidatorKind::Zebrad, &BackendType::State).await;
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn shield() {
            shield_for_validator::<StateService>(&ValidatorKind::Zebrad, &BackendType::State).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn monitor_unverified_mempool() {
            monitor_unverified_mempool_for_validator::<StateService>(
                &ValidatorKind::Zebrad,
                &BackendType::State,
            )
            .await;
        }
    }
}
