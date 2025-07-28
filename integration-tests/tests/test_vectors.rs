//! Holds code used to build test vector data for unit tests. These tests should not be run by default or in CI.

use core2::io::{self, Read, Write};
use prost::Message;
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::io::BufWriter;
use std::path::Path;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_state::read_u32_le;
use zaino_state::write_u32_le;
use zaino_state::CompactSize;
use zaino_state::ZainoVersionedSerialise;
use zaino_state::{BackendType, ChainBlock, ChainWork};
use zaino_state::{
    StateService, StateServiceConfig, StateServiceSubscriber, ZcashIndexer, ZcashService as _,
};
use zaino_testutils::from_inputs;
use zaino_testutils::services;
use zaino_testutils::Validator as _;
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::parameters::Network;
use zebra_rpc::methods::GetAddressUtxos;
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetBlockTransaction};

async fn create_test_manager_and_services(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
    network: Option<services::network::Network>,
) -> (TestManager, StateService, StateServiceSubscriber) {
    let test_manager = TestManager::launch(
        validator,
        &BackendType::Fetch,
        network,
        chain_cache.clone(),
        enable_zaino,
        false,
        false,
        true,
        true,
        enable_clients,
    )
    .await
    .unwrap();

    let (network_type, _zaino_sync_bool) = match network {
        Some(services::network::Network::Mainnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (Network::Mainnet, false)
        }
        Some(services::network::Network::Testnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (Network::new_default_testnet(), false)
        }
        _ => (
            Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // TODO: What is network upgrade 6.1? What does a minor version NU mean?
                    nu6_1: None,
                    nu7: None,
                },
            ),
            true,
        ),
    };

    test_manager.local_net.print_stdout();

    let state_chain_cache_dir = match chain_cache {
        Some(dir) => dir,
        None => test_manager.data_dir.clone(),
    };

    let state_service = StateService::spawn(StateServiceConfig::new(
        zebra_state::Config {
            cache_dir: state_chain_cache_dir,
            ephemeral: false,
            delete_old_database: true,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
        },
        test_manager.zebrad_rpc_listen_address,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        test_manager
            .local_net
            .data_dir()
            .path()
            .to_path_buf()
            .join("zaino"),
        None,
        network_type,
        true,
        true,
    ))
    .await
    .unwrap();

    let state_subscriber = state_service.get_subscriber().inner();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (test_manager, state_service, state_subscriber)
}

#[tokio::test]
#[ignore = "Not a test! Used to build test vector data for zaino_state::chain_index unit tests."]
async fn create_200_block_regtest_chain_vectors() {
    let (mut test_manager, _state_service, state_service_subscriber) =
        create_test_manager_and_services(&ValidatorKind::Zebrad, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    let faucet_taddr = clients.get_faucet_address("transparent").await;
    let faucet_saddr = clients.get_faucet_address("sapling").await;
    let faucet_uaddr = clients.get_faucet_address("unified").await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let recipient_saddr = clients.get_recipient_address("sapling").await;
    let recipient_uaddr = clients.get_recipient_address("unified").await;

    clients.faucet.sync_and_await().await.unwrap();

    // *** Mine 100 blocks to finalise first block reward ***
    test_manager.local_net.generate_blocks(100).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // *** Build 100 block chain holding transparent, sapling, and orchard transactions ***
    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();

    // create transactions
    clients.faucet.quick_shield().await.unwrap();

    // Generate block
    test_manager.local_net.generate_blocks(1).await.unwrap(); // Block 102
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();

    // create transactions
    clients.faucet.quick_shield().await.unwrap();
    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_uaddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();

    // Generate block
    test_manager.local_net.generate_blocks(1).await.unwrap(); // Block 103
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();
    clients.recipient.sync_and_await().await.unwrap();

    // create transactions
    clients.faucet.quick_shield().await.unwrap();

    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_uaddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();

    from_inputs::quick_send(
        &mut clients.recipient,
        vec![(faucet_taddr.as_str(), 200_000, None)],
    )
    .await
    .unwrap();

    // Generate block
    test_manager.local_net.generate_blocks(1).await.unwrap(); // Block 104
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();
    clients.recipient.sync_and_await().await.unwrap();

    // create transactions
    clients.faucet.quick_shield().await.unwrap();
    clients.recipient.quick_shield().await.unwrap();

    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_uaddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();

    from_inputs::quick_send(
        &mut clients.recipient,
        vec![(faucet_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();

    // Generate block
    test_manager.local_net.generate_blocks(1).await.unwrap(); // Block 105
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for _i in 0..48 {
        // sync wallets
        clients.faucet.sync_and_await().await.unwrap();
        clients.recipient.sync_and_await().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap());
        if chain_height.0 >= 200 {
            break;
        }

        // create transactions
        clients.faucet.quick_shield().await.unwrap();
        clients.recipient.quick_shield().await.unwrap();

        from_inputs::quick_send(
            &mut clients.faucet,
            vec![(recipient_taddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &mut clients.faucet,
            vec![(recipient_uaddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();

        from_inputs::quick_send(
            &mut clients.recipient,
            vec![(faucet_taddr.as_str(), 200_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &mut clients.recipient,
            vec![(faucet_uaddr.as_str(), 200_000, None)],
        )
        .await
        .unwrap();

        // Generate block
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // sync wallets
        clients.faucet.sync_and_await().await.unwrap();
        clients.recipient.sync_and_await().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap());
        if chain_height.0 >= 200 {
            break;
        }

        // create transactions
        clients.faucet.quick_shield().await.unwrap();
        clients.recipient.quick_shield().await.unwrap();

        from_inputs::quick_send(
            &mut clients.faucet,
            vec![(recipient_taddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &mut clients.faucet,
            vec![(recipient_saddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &mut clients.faucet,
            vec![(recipient_uaddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();

        from_inputs::quick_send(
            &mut clients.recipient,
            vec![(faucet_taddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        from_inputs::quick_send(
            &mut clients.recipient,
            vec![(faucet_saddr.as_str(), 250_000, None)],
        )
        .await
        .unwrap();

        // Generate block
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(10000)).await;

    // *** Fetch chain data ***
    let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap());

    //fetch  and build block data
    let block_data = {
        let mut data = Vec::new();
        let mut parent_chain_work = ChainWork::from_u256(0.into());
        let mut parent_block_sapling_tree_size: u32 = 0;
        let mut parent_block_orchard_tree_size: u32 = 0;

        for height in 1..=chain_height.0 {
            let (chain_block, compact_block) = {
                // Fetch block data
                let (_hash, tx, trees) = state_service_subscriber
                    .z_get_block(height.to_string(), Some(1))
                    .await
                    .and_then(|response| match response {
                        zebra_rpc::methods::GetBlock::Raw(_) => {
                            Err(zaino_state::StateServiceError::Custom(
                                "Found transaction of `Raw` type, expected only `Object` types."
                                    .to_string(),
                            ))
                        }
                        zebra_rpc::methods::GetBlock::Object {
                            hash,
                            confirmations: _,
                            size: _,
                            height: _,
                            version: _,
                            merkle_root: _,
                            block_commitments: _,
                            final_sapling_root: _,
                            final_orchard_root: _,
                            tx,
                            time: _,
                            nonce: _,
                            solution: _,
                            bits: _,
                            difficulty: _,
                            trees,
                            previous_block_hash: _,
                            next_block_hash: _,
                        } => Ok((
                            hash.0 .0,
                            tx.into_iter()
                                .map(|item| {
                                    match item {
                                        GetBlockTransaction::Hash(h) => Ok(h.0.to_vec()),
                                        GetBlockTransaction::Object(_) => Err(
                                            zaino_state::StateServiceError::Custom(
                                                "Found transaction of `Object` type, expected only `Hash` types."
                                                    .to_string(),
                                            ),
                                        ),
                                    }
                                })
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap(),
                            (trees.sapling(), trees.orchard()),
                        )),
                    })
                    .unwrap();
                let (sapling, orchard) = trees;

                let block_data = state_service_subscriber
                    .z_get_block(height.to_string(), Some(0))
                    .await
                    .and_then(|response| match response {
                        zebra_rpc::methods::GetBlock::Object { .. } => {
                            Err(zaino_state::StateServiceError::Custom(
                                "Found transaction of `Object` type, expected only `Raw` types."
                                    .to_string(),
                            ))
                        }
                        zebra_rpc::methods::GetBlock::Raw(block_hex) => Ok(block_hex),
                    })
                    .unwrap();

                // TODO: Fetch real roots (must be calculated from treestate since removed from spec).
                let (sapling_root, orchard_root): ([u8; 32], [u8; 32]) = { ([0u8; 32], [1u8; 32]) };

                // Build block data
                let full_block = zaino_fetch::chain::block::FullBlock::parse_from_hex(
                    block_data.as_ref(),
                    Some(display_txids_to_server(tx.clone())),
                )
                .unwrap();

                let chain_block = ChainBlock::try_from((
                    full_block.clone(),
                    parent_chain_work,
                    sapling_root,
                    orchard_root,
                    parent_block_sapling_tree_size,
                    parent_block_orchard_tree_size,
                ))
                .unwrap();

                let compact_block = full_block
                    .clone()
                    .into_compact(sapling.try_into().unwrap(), orchard.try_into().unwrap())
                    .unwrap();

                (chain_block, compact_block)
            };
            parent_block_sapling_tree_size = chain_block.commitment_tree_data().sizes().sapling();
            parent_block_orchard_tree_size = chain_block.commitment_tree_data().sizes().orchard();
            parent_chain_work = *chain_block.index().chainwork();
            data.push((height, chain_block, compact_block));
        }
        data
    };

    // Fetch and build wallet addr transparent data
    let faucet_data = {
        let faucet_txids = state_service_subscriber
            .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
                vec![faucet_taddr.clone()],
                1,
                chain_height.0,
            ))
            .await
            .unwrap();

        let faucet_utxos = state_service_subscriber
            .z_get_address_utxos(AddressStrings::new_valid(vec![faucet_taddr.clone()]).unwrap())
            .await
            .unwrap();

        let faucet_balance = state_service_subscriber
            .z_get_address_balance(AddressStrings::new_valid(vec![faucet_taddr.clone()]).unwrap())
            .await
            .unwrap()
            .balance;

        (faucet_txids, faucet_utxos, faucet_balance)
    };

    // fetch recipient addr transparent data
    let recipient_data = {
        let recipient_txids = state_service_subscriber
            .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
                vec![recipient_taddr.clone()],
                1,
                chain_height.0,
            ))
            .await
            .unwrap();

        let recipient_utxos = state_service_subscriber
            .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_taddr.clone()]).unwrap())
            .await
            .unwrap();

        let recipient_balance = state_service_subscriber
            .z_get_address_balance(
                AddressStrings::new_valid(vec![recipient_taddr.clone()]).unwrap(),
            )
            .await
            .unwrap()
            .balance;

        (recipient_txids, recipient_utxos, recipient_balance)
    };

    // *** Save chain vectors to disk ***

    let vec_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("vectors_tmp");
    if vec_dir.exists() {
        fs::remove_dir_all(&vec_dir).unwrap();
    }

    write_vectors_to_file(&vec_dir, &block_data, &faucet_data, &recipient_data).unwrap();

    // *** Read data from files to validate write format.

    let (re_blocks, re_faucet, re_recipient) = read_vectors_from_file(&vec_dir).unwrap();

    for ((h_orig, chain_orig, compact_orig), (h_new, chain_new, compact_new)) in
        block_data.iter().zip(re_blocks.iter())
    {
        assert_eq!(h_orig, h_new, "height mismatch at block {h_orig}");
        assert_eq!(
            chain_orig.to_bytes().unwrap(),
            chain_new.to_bytes().unwrap(),
            "ChainBlock serialisation mismatch at height {h_orig}"
        );
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        compact_orig.encode(&mut buf1).unwrap();
        compact_new.encode(&mut buf2).unwrap();
        assert_eq!(
            buf1, buf2,
            "CompactBlock protobuf mismatch at height {h_orig}"
        );
    }

    assert_eq!(faucet_data, re_faucet, "faucet tuple mismatch");
    assert_eq!(recipient_data, re_recipient, "recipient tuple mismatch");
}

/// Test-only helper: takes big-endian hex‐encoded txids (`Vec<Vec<u8>>`)
/// and returns them as little-endian raw-byte vectors.
fn display_txids_to_server(txids: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    txids
        .into_iter()
        .map(|mut t| {
            t.reverse();
            t
        })
        .collect()
}

pub fn write_vectors_to_file<P: AsRef<Path>>(
    base_dir: P,
    block_data: &[(u32, ChainBlock, CompactBlock)],
    faucet_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
    recipient_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
) -> io::Result<()> {
    let base = base_dir.as_ref();
    fs::create_dir_all(base)?;

    let mut cb_out = BufWriter::new(File::create(base.join("chain_blocks.dat"))?);
    for (h, chain, _) in block_data {
        write_u32_le(&mut cb_out, *h)?;
        let payload = chain.to_bytes()?; // <── new
        CompactSize::write(&mut cb_out, payload.len())?;
        cb_out.write_all(&payload)?;
    }

    let mut cp_out = BufWriter::new(File::create(base.join("compact_blocks.dat"))?);
    for (h, _, compact) in block_data {
        write_u32_le(&mut cp_out, *h)?;
        let mut buf = Vec::with_capacity(compact.encoded_len());
        compact.encode(&mut buf).unwrap();
        CompactSize::write(&mut cp_out, buf.len())?;
        cp_out.write_all(&buf)?;
    }

    serde_json::to_writer_pretty(File::create(base.join("faucet_data.json"))?, faucet_data)?;
    serde_json::to_writer_pretty(
        File::create(base.join("recipient_data.json"))?,
        recipient_data,
    )?;

    Ok(())
}

pub fn read_vectors_from_file<P: AsRef<Path>>(
    base_dir: P,
) -> io::Result<(
    Vec<(u32, ChainBlock, CompactBlock)>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    let base = base_dir.as_ref();

    let mut chain_blocks = Vec::<(u32, ChainBlock)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("chain_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };
            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            let chain = ChainBlock::from_bytes(&buf)?; // <── new
            chain_blocks.push((height, chain));
        }
    }

    let mut full_blocks = Vec::<(u32, ChainBlock, CompactBlock)>::with_capacity(chain_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("compact_blocks.dat"))?);
        for (h1, chain) in chain_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch between ChainBlock and CompactBlock streams",
                ));
            }
            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            let compact = CompactBlock::decode(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            full_blocks.push((h1, chain, compact));
        }
    }

    let faucet = serde_json::from_reader(File::open(base.join("faucet_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let recipient = serde_json::from_reader(File::open(base.join("recipient_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok((full_blocks, faucet, recipient))
}
