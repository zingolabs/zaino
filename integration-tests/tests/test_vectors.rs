//! Holds code used to build test vector data for unit tests. These tests should not be run by default or in CI.

use anyhow::Context;
use core2::io::{self, Read, Write};
use futures::TryFutureExt as _;
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use tower::{Service, ServiceExt as _};
use zaino_fetch::chain::transaction::FullTransaction;
use zaino_fetch::chain::utils::ParseFromSlice;
use zaino_state::read_u32_le;
use zaino_state::read_u64_le;
use zaino_state::write_u32_le;
use zaino_state::write_u64_le;
use zaino_state::CompactSize;
#[allow(deprecated)]
use zaino_state::StateService;
use zaino_state::ZcashIndexer;
use zaino_state::{BackendType, ChainWork, IndexedBlock};
use zaino_testutils::from_inputs;
use zaino_testutils::test_vectors::transactions::get_test_vectors;
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_rpc::methods::GetAddressUtxos;
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetBlockTransaction};
use zebra_state::HashOrHeight;
use zebra_state::{ReadRequest, ReadResponse};

macro_rules! expected_read_response {
    ($response:ident, $expected_variant:ident) => {
        match $response {
            ReadResponse::$expected_variant(inner) => inner,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "Not a test! Used to build test vector data for zaino_state::chain_index unit tests."]
#[allow(deprecated)]
async fn create_200_block_regtest_chain_vectors() {
    let mut test_manager = TestManager::<StateService>::launch(
        &ValidatorKind::Zebrad,
        &BackendType::State,
        None,
        None,
        None,
        true,
        false,
        true,
    )
    .await
    .unwrap();

    let state_service_subscriber = test_manager.service_subscriber.take().unwrap();

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
    test_manager
        .generate_blocks_and_poll_indexer(100, &state_service_subscriber)
        .await;

    // *** Build 100 block chain holding transparent, sapling, and orchard transactions ***
    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();

    // create transactions
    clients
        .faucet
        .quick_shield(zip32::AccountId::ZERO)
        .await
        .unwrap();

    // Generate block
    test_manager
        .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
        .await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();

    // create transactions
    clients
        .faucet
        .quick_shield(zip32::AccountId::ZERO)
        .await
        .unwrap();
    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_uaddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();

    // Generate block
    test_manager
        .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
        .await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();
    clients.recipient.sync_and_await().await.unwrap();

    // create transactions
    clients
        .faucet
        .quick_shield(zip32::AccountId::ZERO)
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

    // Generate block
    test_manager
        .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
        .await;

    // sync wallets
    clients.faucet.sync_and_await().await.unwrap();
    clients.recipient.sync_and_await().await.unwrap();

    // create transactions
    clients
        .faucet
        .quick_shield(zip32::AccountId::ZERO)
        .await
        .unwrap();
    clients
        .recipient
        .quick_shield(zip32::AccountId::ZERO)
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
    test_manager
        .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
        .await;

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
        clients
            .faucet
            .quick_shield(zip32::AccountId::ZERO)
            .await
            .unwrap();
        clients
            .recipient
            .quick_shield(zip32::AccountId::ZERO)
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
        test_manager
            .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
            .await;

        // sync wallets
        clients.faucet.sync_and_await().await.unwrap();
        clients.recipient.sync_and_await().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap());
        if chain_height.0 >= 200 {
            break;
        }

        // create transactions
        clients
            .faucet
            .quick_shield(zip32::AccountId::ZERO)
            .await
            .unwrap();
        clients
            .recipient
            .quick_shield(zip32::AccountId::ZERO)
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
        test_manager
            .generate_blocks_and_poll_indexer(1, &state_service_subscriber)
            .await;
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
            let (chain_block, zebra_block, block_roots, block_treestate) = {
                // Fetch block data
                let (_hash, tx, _trees) = state_service_subscriber
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

                let mut state = state_service_subscriber.read_state_service.clone();
                let (sapling_root, orchard_root) = {
                    let (sapling_tree_response, orchard_tree_response) = futures::future::join(
                        state.clone().call(zebra_state::ReadRequest::SaplingTree(
                            HashOrHeight::Height(zebra_chain::block::Height(height)),
                        )),
                        state.clone().call(zebra_state::ReadRequest::OrchardTree(
                            HashOrHeight::Height(zebra_chain::block::Height(height)),
                        )),
                    )
                    .await;
                    let (sapling_tree, orchard_tree) = match (
                        //TODO: Better readstateservice error handling
                        sapling_tree_response.unwrap(),
                        orchard_tree_response.unwrap(),
                    ) {
                        (
                            zebra_state::ReadResponse::SaplingTree(saptree),
                            zebra_state::ReadResponse::OrchardTree(orctree),
                        ) => (saptree, orctree),
                        (_, _) => panic!("Bad response"),
                    };

                    (
                        sapling_tree
                            .as_deref()
                            .map(|tree| (tree.root(), tree.count()))
                            .unwrap(),
                        orchard_tree
                            .as_deref()
                            .map(|tree| (tree.root(), tree.count()))
                            .unwrap(),
                    )
                };

                let sapling_treestate = match zebra_chain::parameters::NetworkUpgrade::Sapling
                    .activation_height(&state_service_subscriber.network().to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height.0 => Some(
                        state
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::SaplingTree(HashOrHeight::Height(
                                    zebra_chain::block::Height(height),
                                )))
                            })
                            .await
                            .unwrap(),
                    ),
                    _ => Some(zebra_state::ReadResponse::SaplingTree(Some(Arc::new(
                        zebra_chain::sapling::tree::NoteCommitmentTree::default(),
                    )))),
                }
                .and_then(|sap_response| {
                    expected_read_response!(sap_response, SaplingTree)
                        .map(|tree| tree.to_rpc_bytes())
                })
                .unwrap();
                let orchard_treestate = match zebra_chain::parameters::NetworkUpgrade::Nu5
                    .activation_height(&state_service_subscriber.network().to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height.0 => Some(
                        state
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::OrchardTree(HashOrHeight::Height(
                                    zebra_chain::block::Height(height),
                                )))
                            })
                            .await
                            .unwrap(),
                    ),
                    _ => Some(zebra_state::ReadResponse::OrchardTree(Some(Arc::new(
                        zebra_chain::orchard::tree::NoteCommitmentTree::default(),
                    )))),
                }
                .and_then(|orch_response| {
                    expected_read_response!(orch_response, OrchardTree)
                        .map(|tree| tree.to_rpc_bytes())
                })
                .unwrap();

                // Build block data
                let full_block = zaino_fetch::chain::block::FullBlock::parse_from_hex(
                    block_data.as_ref(),
                    Some(display_txids_to_server(tx.clone())),
                )
                .unwrap();

                let chain_block = IndexedBlock::try_from((
                    full_block.clone(),
                    parent_chain_work,
                    sapling_root.0.into(),
                    orchard_root.0.into(),
                    parent_block_sapling_tree_size,
                    parent_block_orchard_tree_size,
                ))
                .unwrap();

                let zebra_block =
                    zebra_chain::block::Block::zcash_deserialize(block_data.as_ref()).unwrap();

                let block_roots = (
                    sapling_root.0,
                    chain_block.commitment_tree_data().sizes().sapling() as u64,
                    orchard_root.0,
                    chain_block.commitment_tree_data().sizes().orchard() as u64,
                );

                let block_treestate = (sapling_treestate, orchard_treestate);

                (chain_block, zebra_block, block_roots, block_treestate)
            };

            // Update parent block
            parent_block_sapling_tree_size = chain_block.commitment_tree_data().sizes().sapling();
            parent_block_orchard_tree_size = chain_block.commitment_tree_data().sizes().orchard();
            parent_chain_work = *chain_block.index().chainwork();

            data.push((height, zebra_block, block_roots, block_treestate));
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

    for ((h_orig, zebra_orig, roots_orig, trees_orig), (h_new, zebra_new, roots_new, trees_new)) in
        block_data.iter().zip(re_blocks.iter())
    {
        assert_eq!(h_orig, h_new, "height mismatch at block {h_orig}");
        assert_eq!(
            zebra_orig, zebra_new,
            "zebra_chain::block::Block serialisation mismatch at height {h_orig}"
        );
        assert_eq!(
            roots_orig, roots_new,
            "block root serialisation mismatch at height {h_orig}"
        );
        assert_eq!(
            trees_orig, trees_new,
            "block treestate serialisation mismatch at height {h_orig}"
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
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )],
    faucet_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
    recipient_data: &(Vec<String>, Vec<GetAddressUtxos>, u64),
) -> io::Result<()> {
    let base = base_dir.as_ref();
    fs::create_dir_all(base)?;

    // zcash_blocks.dat
    let mut zb_out = BufWriter::new(File::create(base.join("zcash_blocks.dat"))?);
    for (h, zcash_block, _roots, _treestate) in block_data {
        write_u32_le(&mut zb_out, *h)?;
        let mut bytes = Vec::new();
        zcash_block.zcash_serialize(&mut bytes)?;
        CompactSize::write(&mut zb_out, bytes.len())?;
        zb_out.write_all(&bytes)?;
    }

    // tree_roots.dat
    let mut tr_out = BufWriter::new(File::create(base.join("tree_roots.dat"))?);
    for (h, _blocks, (sapling_root, sapling_size, orchard_root, orchard_size), _treestate) in
        block_data
    {
        write_u32_le(&mut tr_out, *h)?;
        tr_out.write_all(&<[u8; 32]>::from(*sapling_root))?;
        write_u64_le(&mut tr_out, *sapling_size)?;
        tr_out.write_all(&<[u8; 32]>::from(*orchard_root))?;
        write_u64_le(&mut tr_out, *orchard_size)?;
    }

    // tree_states.dat
    let mut ts_out = BufWriter::new(File::create(base.join("tree_states.dat"))?);
    for (h, _blocks, _roots, (sapling_treestate, orchard_treestate)) in block_data {
        write_u32_le(&mut ts_out, *h)?;
        // Write length-prefixed treestate bytes (variable length)
        CompactSize::write(&mut ts_out, sapling_treestate.len())?;
        ts_out.write_all(sapling_treestate)?;
        CompactSize::write(&mut ts_out, orchard_treestate.len())?;
        ts_out.write_all(orchard_treestate)?;
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
        zebra_chain::block::Block,
        (
            zebra_chain::sapling::tree::Root,
            u64,
            zebra_chain::orchard::tree::Root,
            u64,
        ),
        (Vec<u8>, Vec<u8>),
    )>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    let base = base_dir.as_ref();

    // zebra_blocks.dat
    let mut zebra_blocks = Vec::<(u32, zebra_chain::block::Block)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };

            let len: usize = CompactSize::read_t(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;

            let zcash_block = zebra_chain::block::Block::zcash_deserialize(&*buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            zebra_blocks.push((height, zcash_block));
        }
    }

    // tree_roots.dat
    let mut blocks_and_roots = Vec::with_capacity(zebra_blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (height, zebra_block) in zebra_blocks {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
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

            blocks_and_roots.push((
                height,
                zebra_block,
                (sapling_root, sapling_size, orchard_root, orchard_size),
            ));
        }
    }

    // tree_states.dat
    let mut full_data = Vec::with_capacity(blocks_and_roots.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_states.dat"))?);
        for (height, zebra_block, roots) in blocks_and_roots {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_states.dat",
                ));
            }

            let sapling_len: usize = CompactSize::read_t(&mut r)?;
            let mut sapling_state = vec![0u8; sapling_len];
            r.read_exact(&mut sapling_state)?;

            let orchard_len: usize = CompactSize::read_t(&mut r)?;
            let mut orchard_state = vec![0u8; orchard_len];
            r.read_exact(&mut orchard_state)?;

            full_data.push((height, zebra_block, roots, (sapling_state, orchard_state)));
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

#[tokio::test(flavor = "multi_thread")]
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
