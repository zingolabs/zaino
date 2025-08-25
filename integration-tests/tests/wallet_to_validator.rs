//! Holds wallet-to-validator tests for Zaino.

#![forbid(unsafe_code)]

use zaino_state::BackendType;
use zaino_testutils::{
    from_inputs,
    manager::{
        config_builder::ConfigurableBuilder,
        tests::wallet::WalletTestsBuilder,
        traits::{WithClients, WithValidator},
    },
    ValidatorKind,
};

async fn connect_to_node_get_info_for_validator(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.faucet().do_info().await;
    manager.recipient().do_info().await;
}

async fn send_to_orchard(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.faucet().sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let recipient_ua = manager.get_recipient_address(ClientAddressType::Unified).await;
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();
    manager.generate_blocks_with_delay(1).await;
    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .orchard_balance
            .unwrap(),
        250_000
    );
}

async fn send_to_sapling(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.faucet().sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let recipient_zaddr = manager.get_recipient_address(ClientAddressType::Sapling).await;
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_zaddr, 250_000, None)])
        .await
        .unwrap();
    manager.generate_blocks_with_delay(1).await;
    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .sapling_balance
            .unwrap(),
        250_000
    );
}

async fn send_to_transparent(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.faucet().sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let recipient_taddr = manager.get_recipient_address(ClientAddressType::Transparent).await;
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();

    manager.generate_blocks_with_delay(1).await;

    let fetch_service = manager.create_json_connector().await.unwrap();

    println!("\n\nFetching Chain Height!\n");

    let height = dbg!(fetch_service.get_blockchain_info().await.unwrap().blocks.0);

    println!("\n\nFetching Tx From Unfinalized Chain!\n");

    let unfinalised_transactions = fetch_service
        .get_address_txids(
            vec![manager.get_recipient_address(ClientAddressType::Transparent).await],
            height,
            height,
        )
        .await
        .unwrap();

    dbg!(unfinalised_transactions.clone());

    // Generate blocks
    //
    // NOTE: Generating blocks with zcashd blocks the tokio main thread???,
    //       stopping background processes from running,
    //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
    for height in 1..=99 {
        dbg!("Generating block at height: {}", height);
        manager.generate_blocks_with_delay(1).await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    println!("\n\nFetching Tx From Finalized Chain!\n");

    let finalised_transactions = fetch_service
        .get_address_txids(
            vec![manager.get_recipient_address(ClientAddressType::Transparent).await],
            height,
            height,
        )
        .await
        .unwrap();

    dbg!(finalised_transactions.clone());

    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .confirmed_transparent_balance
            .unwrap(),
        250_000
    );

    assert_eq!(unfinalised_transactions, finalised_transactions);
    // manager.local_net.print_stdout();
}


async fn send_to_all(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.generate_blocks_with_delay(2).await;
    manager.faucet().sync_and_await().await.unwrap();

    // "Create" 3 orchard notes in faucet.
    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let recipient_ua = manager.get_recipient_address(ClientAddressType::Unified).await;
    let recipient_zaddr = manager.get_recipient_address(ClientAddressType::Sapling).await;
    let recipient_taddr = manager.get_recipient_address(ClientAddressType::Transparent).await;
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_zaddr, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();

    // Generate blocks
    //
    // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
    //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
    for height in 1..=100 {
        dbg!("Generating block at height: {}", height);
        manager.generate_blocks_with_delay(1).await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .orchard_balance
            .unwrap(),
        250_000
    );
    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .sapling_balance
            .unwrap(),
        250_000
    );
    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .confirmed_transparent_balance
            .unwrap(),
        250_000
    );
}

async fn shield_for_validator(validator: &ValidatorKind, backend: &BackendType) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.faucet().sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let recipient_taddr = manager.get_recipient_address(ClientAddressType::Transparent).await;
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();

    // Generate blocks
    //
    // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
    //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
    for height in 1..=100 {
        dbg!("Generating block at height: {}", height);
        manager.generate_blocks_with_delay(1).await;
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .confirmed_transparent_balance
            .unwrap(),
        250_000
    );

    manager.recipient().quick_shield().await.unwrap();
    manager.generate_blocks_with_delay(1).await;
    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .orchard_balance
            .unwrap(),
        235_000
    );
}

async fn monitor_unverified_mempool_for_validator(
    validator: &ValidatorKind,
    backend: &BackendType,
) {
    let manager = WalletTestsBuilder::default()
        .validator(validator.clone())
        .backend(backend.clone())
        .launch()
        .await
        .unwrap();

    manager.generate_blocks_with_delay(1).await;
    manager.faucet().sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(100).await;
        manager.faucet().sync_and_await().await.unwrap();
        manager.faucet().quick_shield().await.unwrap();
        manager.generate_blocks_with_delay(1).await;
        manager.faucet().sync_and_await().await.unwrap();
    };

    let txid_1 = from_inputs::quick_send(
        manager.faucet(),
        vec![(
            &zaino_testutils::get_base_address_macro!(manager.recipient(), ClientAddressType::Unified),
            250_000,
            None,
        )],
    )
    .await
    .unwrap();
    let txid_2 = from_inputs::quick_send(
        manager.faucet(),
        vec![(
            &zaino_testutils::get_base_address_macro!(manager.recipient(), ClientAddressType::Sapling),
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    println!("\n\nStarting Mempool!\n");
    manager.recipient().wallet.lock().await.clear_all();
    manager.recipient().sync_and_await().await.unwrap();

    // manager.local_net.print_stdout();

    let fetch_service = manager.create_json_connector().await.unwrap();

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
        manager
            .recipient()
            .do_balance()
            .await
            .unverified_orchard_balance
            .unwrap(),
        250_000
    );
    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .unverified_sapling_balance
            .unwrap(),
        250_000
    );

    manager.generate_blocks_with_delay(1).await;

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

    manager.recipient().sync_and_await().await.unwrap();

    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .verified_orchard_balance
            .unwrap(),
        250_000
    );
    assert_eq!(
        manager
            .recipient()
            .do_balance()
            .await
            .verified_sapling_balance
            .unwrap(),
        250_000
    );
}

mod zcashd {
    use super::*;

    #[tokio::test]
    async fn connect_to_node_get_info() {
        connect_to_node_get_info_for_validator(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
    }

    mod sent_to {
        use super::*;

        #[tokio::test]
        pub(crate) async fn orchard() {
            send_to_orchard(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test]
        pub(crate) async fn sapling() {
            send_to_sapling(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test]
        pub(crate) async fn transparent() {
            send_to_transparent(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }

        #[tokio::test]
        pub(crate) async fn all() {
            send_to_all(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
        }
    }

    #[tokio::test]
    async fn shield() {
        shield_for_validator(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
    }

    #[tokio::test]
    async fn monitor_unverified_mempool() {
        monitor_unverified_mempool_for_validator(&ValidatorKind::Zcashd, &BackendType::Fetch).await;
    }
}

mod zebrad {
    use super::*;

    mod fetch_service {
        use super::*;

        #[tokio::test]
        async fn connect_to_node_get_info() {
            connect_to_node_get_info_for_validator(&ValidatorKind::Zebrad, &BackendType::Fetch)
                .await;
        }
        mod send_to {
            use super::*;

            #[tokio::test]
            pub(crate) async fn sapling() {
                send_to_sapling(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }

            #[tokio::test]
            pub(crate) async fn orchard() {
                send_to_orchard(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }

            /// Bug documented in https://github.com/zingolabs/zaino/issues/145.
            #[tokio::test]
            pub(crate) async fn transparent() {
                send_to_transparent(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }

            #[tokio::test]
            pub(crate) async fn all() {
                send_to_all(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
            }
        }
        #[tokio::test]
        async fn shield() {
            shield_for_validator(&ValidatorKind::Zebrad, &BackendType::Fetch).await;
        }
        /// Bug documented in https://github.com/zingolabs/zaino/issues/144.
        #[tokio::test]
        async fn monitor_unverified_mempool() {
            monitor_unverified_mempool_for_validator(&ValidatorKind::Zebrad, &BackendType::Fetch)
                .await;
        }
    }

    mod state_service {
        use super::*;

        #[tokio::test]
        async fn connect_to_node_get_info() {
            connect_to_node_get_info_for_validator(&ValidatorKind::Zebrad, &BackendType::State)
                .await;
        }
        mod send_to {
            use super::*;

            #[tokio::test]
            pub(crate) async fn sapling() {
                send_to_sapling(&ValidatorKind::Zebrad, &BackendType::State).await;
            }

            #[tokio::test]
            pub(crate) async fn orchard() {
                send_to_orchard(&ValidatorKind::Zebrad, &BackendType::State).await;
            }

            #[tokio::test]
            pub(crate) async fn transparent() {
                send_to_transparent(&ValidatorKind::Zebrad, &BackendType::State).await;
            }

            #[tokio::test]
            pub(crate) async fn all() {
                send_to_all(&ValidatorKind::Zebrad, &BackendType::State).await;
            }
        }

        #[tokio::test]
        async fn shield() {
            shield_for_validator(&ValidatorKind::Zebrad, &BackendType::State).await;
        }

        #[tokio::test]
        async fn monitor_unverified_mempool() {
            monitor_unverified_mempool_for_validator(&ValidatorKind::Zebrad, &BackendType::State)
                .await;
        }
    }
}
