//! Holds code used to build test vector data for unit tests. These tests should not be run by default or in CI.

use anyhow::Context;
use core2::io::{self, Read, Write};
use prost::Message;
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::io::BufWriter;
use std::path::Path;
use zaino_common::network::ActivationHeights;
use zaino_common::DatabaseConfig;
use zaino_common::ServiceConfig;
use zaino_common::StorageConfig;
use zaino_fetch::chain::transaction::FullTransaction;
use zaino_fetch::chain::utils::ParseFromSlice;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_state::read_u32_le;
use zaino_state::read_u64_le;
use zaino_state::write_u32_le;
use zaino_state::write_u64_le;
use zaino_state::CompactSize;
use zaino_state::ZainoVersionedSerialise;
use zaino_state::{BackendType, ChainWork, IndexedBlock};
use zaino_state::{
    StateService, StateServiceConfig, StateServiceSubscriber, ZcashIndexer, ZcashService as _,
};
use zaino_testutils::from_inputs;
use zaino_testutils::services;
use zaino_testutils::test_vectors::transactions::get_test_vectors;
use zaino_testutils::Validator as _;
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::parameters::NetworkKind;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_rpc::methods::GetAddressUtxos;
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetBlockTransaction};
use zip32::AccountId;

async fn create_test_manager_and_services(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
    network: Option<NetworkKind>,
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

    let (network_type, zaino_sync_bool) = match network {
        Some(NetworkKind::Mainnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (zaino_common::Network::Mainnet, false)
        }
        Some(NetworkKind::Testnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (zaino_common::Network::Testnet, false)
        }
        _ => (
            zaino_common::Network::Regtest(ActivationHeights::default()),
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
        test_manager.zebrad_grpc_listen_address,
        false,
        None,
        None,
        None,
        ServiceConfig::default(),
        StorageConfig {
            database: DatabaseConfig {
                path: test_manager
                    .local_net
                    .data_dir()
                    .path()
                    .to_path_buf()
                    .join("zaino"),
                ..Default::default()
            },
            ..Default::default()
        },
        network_type,
        zaino_sync_bool,
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
    clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();

    // Generate block
    test_manager.local_net.generate_blocks(1).await.unwrap(); // Block 102
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();

    // create transactions
    clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
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
    clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();

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
    clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
    clients
        .recipient
        .quick_shield(AccountId::ZERO)
        .await
        .unwrap();

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
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        clients
            .recipient
            .quick_shield(AccountId::ZERO)
            .await
            .unwrap();

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
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        clients
            .recipient
            .quick_shield(AccountId::ZERO)
            .await
            .unwrap();

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

        for height in 0..=chain_height.0 {
            let (chain_block, compact_block, zebra_block, block_roots) = {
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
                        zebra_rpc::methods::GetBlock::Object(block_obj)  => Ok((
                            block_obj.hash() ,
                            block_obj.tx().iter()
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
                            (block_obj.trees().sapling(), block_obj.trees().orchard()),
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

                // Roots are not easily calculated and are not currently required in unit tests so defaults are used.
                let (sapling_root, orchard_root): ([u8; 32], [u8; 32]) = { ([0u8; 32], [1u8; 32]) };

                // Build block data
                let full_block = zaino_fetch::chain::block::FullBlock::parse_from_hex(
                    block_data.as_ref(),
                    Some(display_txids_to_server(tx.clone())),
                )
                .unwrap();

                let chain_block = IndexedBlock::try_from((
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

                let zebra_block =
                    zebra_chain::block::Block::zcash_deserialize(block_data.as_ref()).unwrap();

                // Roots are not easily calculated and are not currently required in unit tests so defaults are used.
                let block_roots = (
                    zebra_chain::sapling::tree::Root::default(),
                    chain_block.commitment_tree_data().sizes().sapling() as u64,
                    zebra_chain::orchard::tree::Root::default(),
                    chain_block.commitment_tree_data().sizes().orchard() as u64,
                );

                (chain_block, compact_block, zebra_block, block_roots)
            };
            parent_block_sapling_tree_size = chain_block.commitment_tree_data().sizes().sapling();
            parent_block_orchard_tree_size = chain_block.commitment_tree_data().sizes().orchard();
            parent_chain_work = *chain_block.index().chainwork();
            data.push((height, chain_block, compact_block, zebra_block, block_roots));
        }
        data
    };

    // Fetch and build wallet addr transparent data
    let faucet_data = {
        let faucet_txids = state_service_subscriber
            .get_address_tx_ids(GetAddressTxIdsRequest::new(
                vec![faucet_taddr.clone()],
                Some(0),
                Some(chain_height.0),
            ))
            .await
            .unwrap();

        let faucet_utxos = state_service_subscriber
            .z_get_address_utxos(AddressStrings::new(vec![faucet_taddr.clone()]))
            .await
            .unwrap();

        let faucet_balance = state_service_subscriber
            .z_get_address_balance(AddressStrings::new(vec![faucet_taddr.clone()]))
            .await
            .unwrap()
            .balance();

        (faucet_txids, faucet_utxos, faucet_balance)
    };

    // fetch recipient addr transparent data
    let recipient_data = {
        let recipient_txids = state_service_subscriber
            .get_address_tx_ids(GetAddressTxIdsRequest::new(
                vec![recipient_taddr.clone()],
                Some(0),
                Some(chain_height.0),
            ))
            .await
            .unwrap();

        let recipient_utxos = state_service_subscriber
            .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr.clone()]))
            .await
            .unwrap();

        let recipient_balance = state_service_subscriber
            .z_get_address_balance(AddressStrings::new(vec![recipient_taddr.clone()]))
            .await
            .unwrap()
            .balance();

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

    for (
        (h_orig, chain_orig, compact_orig, zebra_orig, roots_orig),
        (h_new, chain_new, compact_new, zebra_new, roots_new),
    ) in block_data.iter().zip(re_blocks.iter())
    {
        assert_eq!(h_orig, h_new, "height mismatch at block {h_orig}");
        assert_eq!(
            chain_orig.to_bytes().unwrap(),
            chain_new.to_bytes().unwrap(),
            "IndexedBlock serialisation mismatch at height {h_orig}"
        );
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        compact_orig.encode(&mut buf1).unwrap();
        compact_new.encode(&mut buf2).unwrap();
        assert_eq!(
            buf1, buf2,
            "CompactBlock protobuf mismatch at height {h_orig}"
        );
        assert_eq!(
            zebra_orig, zebra_new,
            "zebra_chain::block::Block serialisation mismatch at height {h_orig}"
        );
        assert_eq!(
            roots_orig, roots_new,
            "block root serialisation mismatch at height {h_orig}"
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

#[allow(clippy::type_complexity)]
pub fn write_vectors_to_file<P: AsRef<Path>>(
    base_dir: P,
    block_data: &[(
        u32,
        IndexedBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )],
    faucet_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
    recipient_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
) -> io::Result<()> {
    let base = base_dir.as_ref();
    fs::create_dir_all(base)?;

    // chain_blocks.dat
    let mut cb_out = BufWriter::new(File::create(base.join("chain_blocks.dat"))?);
    for (h, chain, _, _, _) in block_data {
        write_u32_le(&mut cb_out, *h)?;
        let payload = chain.to_bytes()?; // <── new
        CompactSize::write(&mut cb_out, payload.len())?;
        cb_out.write_all(&payload)?;
    }

    // compact_blocks.dat
    let mut cp_out = BufWriter::new(File::create(base.join("compact_blocks.dat"))?);
    for (h, _, compact, _, _) in block_data {
        write_u32_le(&mut cp_out, *h)?;
        let mut buf = Vec::with_capacity(compact.encoded_len());
        compact.encode(&mut buf).unwrap();
        CompactSize::write(&mut cp_out, buf.len())?;
        cp_out.write_all(&buf)?;
    }

    // zcash_blocks.dat
    let mut zb_out = BufWriter::new(File::create(base.join("zcash_blocks.dat"))?);
    for (h, _, _, zcash_block, _) in block_data {
        write_u32_le(&mut zb_out, *h)?;
        let mut bytes = Vec::new();
        zcash_block.zcash_serialize(&mut bytes)?;
        CompactSize::write(&mut zb_out, bytes.len())?;
        zb_out.write_all(&bytes)?;
    }

    // tree_roots.dat
    let mut tr_out = BufWriter::new(File::create(base.join("tree_roots.dat"))?);
    for (h, _, _, _, (sapling_root, sapling_size, orchard_root, orchard_size)) in block_data {
        write_u32_le(&mut tr_out, *h)?;
        tr_out.write_all(&<[u8; 32]>::from(*sapling_root))?;
        write_u64_le(&mut tr_out, *sapling_size)?;
        tr_out.write_all(&<[u8; 32]>::from(*orchard_root))?;
        write_u64_le(&mut tr_out, *orchard_size)?;
    }

    // faucet_data.json
    serde_json::to_writer_pretty(File::create(base.join("faucet_data.json"))?, faucet_data)?;

    // recipient_data.json
    serde_json::to_writer_pretty(
        File::create(base.join("recipient_data.json"))?,
        recipient_data,
    )?;

    Ok(())
}

#[allow(clippy::type_complexity)]
pub fn read_vectors_from_file<P: AsRef<Path>>(
    base_dir: P,
) -> io::Result<(
    Vec<(
        u32,
        IndexedBlock,
        CompactBlock,
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    let base = base_dir.as_ref();

    // chain_blocks.dat
    let mut chain_blocks = Vec::<(u32, IndexedBlock)>::new();
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
            let chain = IndexedBlock::from_bytes(&buf)?; // <── new
            chain_blocks.push((height, chain));
        }
    }

    // compact_blocks.dat
    let mut compact_blocks =
        Vec::<(u32, IndexedBlock, CompactBlock)>::with_capacity(chain_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("compact_blocks.dat"))?);
        for (h1, chain) in chain_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch between IndexedBlock and CompactBlock",
                ));
            }
            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            let compact = CompactBlock::decode(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            compact_blocks.push((h1, chain, compact));
        }
    }

    // zebra_blocks.dat
    let mut full_blocks =
        Vec::<(u32, IndexedBlock, CompactBlock, zebra_chain::block::Block)>::with_capacity(
            compact_blocks.len(),
        );
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        for (h1, chain, compact) in compact_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in zcash_blocks.dat",
                ));
            }

            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;

            let zcash_block = zebra_chain::block::Block::zcash_deserialize(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            full_blocks.push((h1, chain, compact, zcash_block));
        }
    }

    // tree_roots.dat
    let mut full_data = Vec::with_capacity(full_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (h1, chain, compact, zcash_block) in full_blocks {
            let h2 = read_u32_le(&mut r)?;
            if h1 != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_roots.dat",
                ));
            }
            let mut sapling_bytes = [0u8; 32];
            r.read_exact(&mut sapling_bytes)?;
            let sapling_root = zebra_chain::sapling::tree::Root::try_from(sapling_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let sapling_size = read_u64_le(&mut r)?;

            let mut orchard_bytes = [0u8; 32];
            r.read_exact(&mut orchard_bytes)?;
            let orchard_root = zebra_chain::orchard::tree::Root::try_from(orchard_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let orchard_size = read_u64_le(&mut r)?;

            full_data.push((
                h1,
                chain,
                compact,
                zcash_block,
                (sapling_root, sapling_size, orchard_root, orchard_size),
            ));
        }
    }

    // faucet_data.json
    let faucet = serde_json::from_reader(File::open(base.join("faucet_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // recipient_data.json
    let recipient = serde_json::from_reader(File::open(base.join("recipient_data.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok((full_data, faucet, recipient))
}

#[tokio::test]
async fn pre_v4_txs_parsing() -> anyhow::Result<()> {
    let test_vectors = get_test_vectors();

    for (i, test_vector) in test_vectors.iter().filter(|v| v.version < 4).enumerate() {
        let description = test_vector.description;
        let version = test_vector.version;
        let raw_tx = test_vector.tx.clone();
        let txid = test_vector.txid;
        // todo!: add an 'is_coinbase' method to the transaction struct to check thid
        let _is_coinbase = test_vector.is_coinbase;
        let has_sapling = test_vector.has_sapling;
        let has_orchard = test_vector.has_orchard;
        let transparent_inputs = test_vector.transparent_inputs;
        let transparent_outputs = test_vector.transparent_outputs;

        let deserialized_tx =
            FullTransaction::parse_from_slice(&raw_tx, Some(vec![txid.to_vec()]), None)
                .with_context(|| {
                    format!("Failed to deserialize transaction with description: {description:?}")
                })?;

        let tx = deserialized_tx.1;

        assert_eq!(
            tx.version(),
            version,
            "Version mismatch for transaction #{i} ({description})"
        );
        assert_eq!(
            tx.tx_id(),
            txid,
            "TXID mismatch for transaction #{i} ({description})"
        );
        // Check Sapling spends (v4+ transactions)
        if version >= 4 {
            assert_eq!(
                !tx.shielded_spends().is_empty(),
                has_sapling != 0,
                "Sapling spends mismatch for transaction #{i} ({description})"
            );
        } else {
            // v1-v3 transactions should not have Sapling spends
            assert!(
                tx.shielded_spends().is_empty(),
                "Transaction #{i} ({description}) version {version} should not have Sapling spends"
            );
        }

        // Check Orchard actions (v5+ transactions)
        if version >= 5 {
            assert_eq!(
                !tx.orchard_actions().is_empty(),
                has_orchard != 0,
                "Orchard actions mismatch for transaction #{i} ({description})"
            );
        } else {
            // v1-v4 transactions should not have Orchard actions
            assert!(
                tx.orchard_actions().is_empty(),
                "Transaction #{i} ({description}) version {version} should not have Orchard actions"
            );
        }
        assert_eq!(
            !tx.transparent_inputs().is_empty(),
            transparent_inputs > 0,
            "Transparent inputs presence mismatch for transaction #{i} ({description})"
        );
        assert_eq!(
            !tx.transparent_outputs().is_empty(),
            transparent_outputs > 0,
            "Transparent outputs presence mismatch for transaction #{i} ({description})"
        );

        // dbg!(tx);
    }
    Ok(())
}
