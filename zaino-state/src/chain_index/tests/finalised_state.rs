//! Zaino-State ChainIndex Finalised State (ZainoDB) unit tests.

use core2::io::{self, Read};
use prost::Message;
use std::io::BufReader;
use std::path::Path;
use std::{fs::File, path::PathBuf};
use tempfile::TempDir;

use zaino_proto::proto::compact_formats::CompactBlock;
use zebra_rpc::methods::GetAddressUtxos;

use crate::bench::BlockCacheConfig;
use crate::chain_index::finalised_state::{DbReader, ZainoDB};
use crate::error::FinalisedStateError;
use crate::{
    read_u32_le, AddrScript, ChainBlock, CompactSize, Height, Outpoint,
    ZainoVersionedSerialise as _,
};

/// Reads test data from file.
fn read_vectors_from_file<P: AsRef<Path>>(
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

fn load_test_vectors() -> io::Result<(
    Vec<(u32, ChainBlock, CompactBlock)>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
)> {
    // <repo>/zaino-state/src/chain_index/tests/vectors
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("chain_index")
        .join("tests")
        .join("vectors");
    read_vectors_from_file(&base_dir)
}

async fn spawn_default_zaino_db() -> Result<(TempDir, ZainoDB), FinalisedStateError> {
    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();

    let config = BlockCacheConfig {
        map_capacity: None,
        map_shard_amount: None,
        db_path,
        db_size: None,
        network: zebra_chain::parameters::Network::new_regtest(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                before_overwinter: Some(1),
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(1),
                nu6: Some(1),
                // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                nu6_1: None,
                nu7: None,
            },
        ),
        no_sync: false,
        no_db: false,
    };

    let zaino_db = ZainoDB::spawn(&config).await.unwrap();

    Ok((temp_dir, zaino_db))
}

async fn load_vectors_and_spawn_and_sync_zaino_db() -> (
    Vec<(u32, ChainBlock, CompactBlock)>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    TempDir,
    ZainoDB,
) {
    let (blocks, faucet, recipient) = load_test_vectors().unwrap();
    let (db_dir, zaino_db) = spawn_default_zaino_db().await.unwrap();
    for (_h, chain_block, _compact_block) in blocks.clone() {
        // dbg!("Writing block at height {}", _h);
        // if _h == 1 {
        //     dbg!(&chain_block);
        // }
        zaino_db.write_block(chain_block).await.unwrap();
    }
    (blocks, faucet, recipient, db_dir, zaino_db)
}

async fn load_vectors_db_and_reader() -> (
    Vec<(u32, ChainBlock, CompactBlock)>,
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    (Vec<String>, Vec<GetAddressUtxos>, u64),
    TempDir,
    std::sync::Arc<ZainoDB>,
    DbReader,
) {
    let (blocks, faucet, recipient, db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_zaino_db().await;

    let zaino_db = std::sync::Arc::new(zaino_db);

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status().await);
    dbg!(zaino_db.tip_height().await.unwrap()).unwrap();

    let db_reader = zaino_db.to_reader();
    dbg!(db_reader.tip_height().await.unwrap()).unwrap();

    (blocks, faucet, recipient, db_dir, zaino_db, db_reader)
}

// *** ZainoDB Tests ***

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vectors_can_be_loaded_and_deserialised() {
    let (blocks, faucet, recipient) = load_test_vectors().unwrap();

    // Chech block data..
    assert!(
        !blocks.is_empty(),
        "expected at least one block in test-vectors"
    );
    let mut prev_h: u32 = 0;
    for (h, chain_block, compact_block) in &blocks {
        // println!("Checking block at height {h}");

        assert_eq!(
            prev_h,
            (h - 1),
            "Chain height continuity check failed at height {h}"
        );
        prev_h = *h;

        let compact_block_hash = compact_block.hash.clone();
        let chain_block_hash_bytes = chain_block.hash().0.to_vec();

        assert_eq!(
            compact_block_hash, chain_block_hash_bytes,
            "Block hash check failed at height {h}"
        );

        // ChainBlock round trip check.
        let bytes = chain_block.to_bytes().unwrap();
        let reparsed = ChainBlock::from_bytes(&bytes).unwrap();
        assert_eq!(
            chain_block, &reparsed,
            "ChainBlock round-trip failed at height {h}"
        );
    }

    // check taddrs.
    let (_, utxos_f, _) = faucet;
    let (_, utxos_r, _) = recipient;

    println!("\nFaucet UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = utxos_f[0].into_parts();
    println!("addr: {}", addr);

    println!("\nRecipient UTXO address:");
    let (addr, _hash, _outindex, _script, _value, _height) = utxos_r[0].into_parts();
    println!("addr: {}", addr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_blocks_to_db_and_verify() {
    let (_blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_zaino_db().await;
    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status().await);
    dbg!(zaino_db.tip_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_blocks_from_db() {
    let (_blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_zaino_db().await;

    for h in (1..=200).rev() {
        // dbg!("Deleting block at height {}", h);
        zaino_db
            .delete_block_at_height(crate::Height(h))
            .await
            .unwrap();
    }

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status().await);
    dbg!(zaino_db.tip_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_db_from_file() {
    let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

    let temp_dir: TempDir = tempfile::tempdir().unwrap();
    let db_path: PathBuf = temp_dir.path().to_path_buf();
    let config = BlockCacheConfig {
        map_capacity: None,
        map_shard_amount: None,
        db_path,
        db_size: None,
        network: zebra_chain::parameters::Network::new_regtest(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                before_overwinter: Some(1),
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(1),
                nu6: Some(1),
                // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                nu6_1: None,
                nu7: None,
            },
        ),
        no_sync: false,
        no_db: false,
    };

    {
        let zaino_db = ZainoDB::spawn(&config).await.unwrap();
        for (_h, chain_block, _compact_block) in blocks.clone() {
            zaino_db.write_block(chain_block).await.unwrap();
        }

        zaino_db.wait_until_ready().await;
        dbg!(zaino_db.status().await);
        dbg!(zaino_db.tip_height().await.unwrap());

        dbg!(zaino_db.close().await.unwrap());
    }

    {
        let zaino_db_2 = ZainoDB::spawn(&config).await.unwrap();

        zaino_db_2.wait_until_ready().await;
        dbg!(zaino_db_2.status().await);
        let db_height = dbg!(zaino_db_2.tip_height().await.unwrap()).unwrap();

        assert_eq!(db_height.0, 200);

        dbg!(zaino_db_2.close().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn try_write_invalid_block() {
    let (blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_zaino_db().await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status().await);
    dbg!(zaino_db.tip_height().await.unwrap());

    let (height, mut chain_block, _compact_block) = blocks.last().unwrap().clone();

    chain_block.index.height = Some(crate::Height(height + 1));
    dbg!(chain_block.index.height);

    let db_err = dbg!(zaino_db.write_block(chain_block).await);

    // TODO: Update with concrete err type.
    assert!(db_err.is_err());

    dbg!(zaino_db.tip_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn try_delete_block_with_invalid_height() {
    let (blocks, _faucet, _recipient, _db_dir, zaino_db) =
        load_vectors_and_spawn_and_sync_zaino_db().await;

    zaino_db.wait_until_ready().await;
    dbg!(zaino_db.status().await);
    dbg!(zaino_db.tip_height().await.unwrap());

    let (height, _chain_block, _compact_block) = blocks.last().unwrap().clone();

    let delete_height = height - 1;

    let db_err = dbg!(
        zaino_db
            .delete_block_at_height(crate::Height(delete_height))
            .await
    );

    // TODO: Update with concrete err type.
    assert!(db_err.is_err());

    dbg!(zaino_db.tip_height().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_db_reader() {
    let (blocks, _faucet, _recipient, _db_dir, zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let (data_height, _, _) = blocks.last().unwrap();
    let db_height = dbg!(zaino_db.tip_height().await.unwrap()).unwrap();
    let db_reader_height = dbg!(db_reader.tip_height().await.unwrap()).unwrap();

    assert_eq!(data_height, &db_height.0);
    assert_eq!(db_height, db_reader_height);
}

// *** DbReader Tests ***

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_chain_blocks() {
    let (blocks, _faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    for (height, chain_block, _) in blocks.iter() {
        let reader_chain_block = db_reader.get_chain_block(Height(*height)).await.unwrap();
        assert_eq!(chain_block, &reader_chain_block);
        println!("ChainBlock at height {} OK", height);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_compact_blocks() {
    let (blocks, _faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    for (height, _, compact_block) in blocks.iter() {
        let reader_compact_block = db_reader.get_compact_block(Height(*height)).await.unwrap();
        assert_eq!(compact_block, &reader_compact_block);
        println!("CompactBlock at height {} OK", height);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_faucet_txids() {
    let (blocks, faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let start = Height(blocks.first().unwrap().0);
    let end = Height(blocks.last().unwrap().0);

    let (faucet_txids, faucet_utxos, _faucet_balance) = faucet;
    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet_utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(&faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    for (height, chain_block, _compact_block) in blocks {
        println!("Checking faucet txids at height {}", height);
        let block_height = Height(height);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| hex::encode(tx_data.txid()))
            .collect();
        let filtered_block_txids: Vec<String> = block_txids
            .into_iter()
            .filter(|txid| faucet_txids.contains(txid))
            .collect();
        dbg!(&filtered_block_txids);

        let reader_faucet_tx_indexes = db_reader
            .addr_tx_indexes_by_range(faucet_addr_script, block_height, block_height)
            .await
            .unwrap()
            .unwrap();
        let mut reader_block_txids = Vec::new();
        for index in reader_faucet_tx_indexes {
            let txid = db_reader.get_txid(index).await.unwrap();
            reader_block_txids.push(txid.to_string());
        }
        dbg!(&reader_block_txids);

        assert_eq!(filtered_block_txids.len(), reader_block_txids.len());
        assert_eq!(filtered_block_txids, reader_block_txids);
    }

    println!("Checking full faucet data");
    let reader_faucet_tx_indexes = db_reader
        .addr_tx_indexes_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();
    let mut reader_faucet_txids = Vec::new();
    for index in reader_faucet_tx_indexes {
        let txid = db_reader.get_txid(index).await.unwrap();
        reader_faucet_txids.push(txid.to_string());
    }

    assert_eq!(faucet_txids.len(), reader_faucet_txids.len());
    assert_eq!(faucet_txids, reader_faucet_txids);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_recipient_txids() {
    let (blocks, _faucet, recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let start = Height(blocks.first().unwrap().0);
    let end = Height(blocks.last().unwrap().0);

    let (recipient_txids, recipient_utxos, _recipient_balance) = recipient;
    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient_utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(&recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    for (height, chain_block, _compact_block) in blocks {
        println!("Checking recipient txids at height {}", height);
        let block_height = Height(height);
        let block_txids: Vec<String> = chain_block
            .transactions()
            .iter()
            .map(|tx_data| hex::encode(tx_data.txid()))
            .collect();

        // Get block txids that are relevant to recipient.
        let filtered_block_txids: Vec<String> = block_txids
            .into_iter()
            .filter(|txid| recipient_txids.contains(txid))
            .collect();
        dbg!(&filtered_block_txids);

        let reader_recipient_tx_indexes = match db_reader
            .addr_tx_indexes_by_range(recipient_addr_script, block_height, block_height)
            .await
            .unwrap()
        {
            Some(v) => v,
            None => continue,
        };
        let mut reader_block_txids = Vec::new();
        for index in reader_recipient_tx_indexes {
            let txid = db_reader.get_txid(index).await.unwrap();
            reader_block_txids.push(txid.to_string());
        }
        dbg!(&reader_block_txids);

        assert_eq!(filtered_block_txids.len(), reader_block_txids.len());
        assert_eq!(filtered_block_txids, reader_block_txids);
    }

    println!("Checking full faucet data");
    let reader_recipient_tx_indexes = db_reader
        .addr_tx_indexes_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_recipient_txids = Vec::new();
    for index in reader_recipient_tx_indexes {
        let txid = db_reader.get_txid(index).await.unwrap();
        reader_recipient_txids.push(txid.to_string());
    }

    assert_eq!(recipient_txids.len(), reader_recipient_txids.len());
    assert_eq!(recipient_txids, reader_recipient_txids);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_faucet_utxos() {
    let (blocks, faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let start = Height(blocks.first().unwrap().0);
    let end = Height(blocks.last().unwrap().0);

    let (_faucet_txids, faucet_utxos, _faucet_balance) = faucet;
    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet_utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(&faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut cleaned_utxos = Vec::new();
    for utxo in faucet_utxos.iter() {
        let (_faucet_address, txid, output_index, _faucet_script, satoshis, _height) =
            utxo.into_parts();
        cleaned_utxos.push((txid.to_string(), output_index.index(), satoshis));
    }

    let reader_faucet_utxo_indexes = db_reader
        .addr_utxos_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_faucet_utxos = Vec::new();

    for (index, vout, value) in reader_faucet_utxo_indexes {
        let txid = db_reader.get_txid(index).await.unwrap().to_string();
        reader_faucet_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_faucet_utxos.len());
    assert_eq!(cleaned_utxos, reader_faucet_utxos);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_recipient_utxos() {
    let (blocks, _faucet, recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let start = Height(blocks.first().unwrap().0);
    let end = Height(blocks.last().unwrap().0);

    let (_recipient_txids, recipient_utxos, _recipient_balance) = recipient;
    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient_utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(&recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let mut cleaned_utxos = Vec::new();
    for utxo in recipient_utxos.iter() {
        let (_recipient_address, txid, output_index, _recipient_script, satoshis, _height) =
            utxo.into_parts();
        cleaned_utxos.push((txid.to_string(), output_index.index(), satoshis));
    }

    let reader_recipient_utxo_indexes = db_reader
        .addr_utxos_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()
        .unwrap();

    let mut reader_recipient_utxos = Vec::new();

    for (index, vout, value) in reader_recipient_utxo_indexes {
        let txid = db_reader.get_txid(index).await.unwrap().to_string();
        reader_recipient_utxos.push((txid, vout as u32, value));
    }

    assert_eq!(cleaned_utxos.len(), reader_recipient_utxos.len());
    assert_eq!(cleaned_utxos, reader_recipient_utxos);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_balance() {
    let (blocks, faucet, recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let start = Height(blocks.first().unwrap().0);
    let end = Height(blocks.last().unwrap().0);

    // Check faucet

    let (_faucet_txids, faucet_utxos, faucet_balance) = faucet;
    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet_utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(&faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let reader_faucet_balance = dbg!(db_reader
        .addr_balance_by_range(faucet_addr_script, start, end)
        .await
        .unwrap()) as u64;

    assert_eq!(faucet_balance, reader_faucet_balance);

    // Check recipient

    let (_recipient_txids, recipient_utxos, recipient_balance) = recipient;
    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient_utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(&recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    let reader_recipient_balance = dbg!(db_reader
        .addr_balance_by_range(recipient_addr_script, start, end)
        .await
        .unwrap()) as u64;

    assert_eq!(recipient_balance, reader_recipient_balance);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_faucet_spent_map() {
    let (blocks, faucet, _recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let (_faucet_txids, faucet_utxos, _faucet_balance) = faucet;
    let (_faucet_address, _txid, _output_index, faucet_script, _satoshis, _height) =
        faucet_utxos.first().unwrap().into_parts();
    let faucet_addr_script = AddrScript::from_script(&faucet_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    // collect faucet outpoints
    let mut faucet_outpoints = Vec::new();
    let mut faucet_ouptpoints_spent_status = Vec::new();
    for (_height, chain_block, _compact_block) in blocks.clone() {
        for tx in chain_block.transactions() {
            let txid = tx.txid();
            let outputs = tx.transparent().outputs();
            for (vout_idx, output) in outputs.iter().enumerate() {
                if output.script_hash() == faucet_addr_script.hash() {
                    let outpoint = Outpoint::new_from_be(txid, vout_idx as u32);

                    let spender = db_reader.get_outpoint_spender(outpoint).await.unwrap();

                    faucet_outpoints.push(outpoint);
                    faucet_ouptpoints_spent_status.push(spender);
                }
            }
        }
    }

    // collect faucet txids holding utxos
    let mut faucet_utxo_indexes = Vec::new();
    for utxo in faucet_utxos.iter() {
        let (_faucet_address, txid, output_index, _faucet_script, _satoshis, _height) =
            utxo.into_parts();
        faucet_utxo_indexes.push((txid.to_string(), output_index.index()));
    }

    // check full spent outpoints map
    let faucet_spent_map = db_reader
        .get_outpoint_spenders(faucet_outpoints.clone())
        .await
        .unwrap();
    assert_eq!(&faucet_ouptpoints_spent_status, &faucet_spent_map);

    for (outpoint, spender_option) in faucet_outpoints
        .iter()
        .zip(faucet_ouptpoints_spent_status.iter())
    {
        let outpoint_tuple = (
            crate::Hash::from(*outpoint.prev_txid()).to_string(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = blocks.iter().find_map(|(_h, chain_block, _cb)| {
                    chain_block.transactions().iter().find(|tx| {
                        let (block_height, tx_idx) =
                            (spender_index.block_index(), spender_index.tx_index());
                        chain_block.index().height() == Some(Height(block_height))
                            && tx.index() == tx_idx as u64
                    })
                });
                assert!(
                    spender_tx.is_some(),
                    "Spender transaction not found in blocks!"
                );

                let spender_tx = spender_tx.unwrap();
                let matches = spender_tx.transparent().inputs().iter().any(|input| {
                    input.prevout_txid() == outpoint.prev_txid()
                        && input.prevout_index() == outpoint.prev_index()
                });
                assert!(
                    matches,
                    "Spender transaction does not actually spend the outpoint: {:?}",
                    outpoint
                );

                assert!(
                    !faucet_utxo_indexes.contains(&outpoint_tuple),
                    "Spent outpoint should NOT be in UTXO set, but found: {:?}",
                    outpoint_tuple
                );
            }
            None => {
                assert!(
                    faucet_utxo_indexes.contains(&outpoint_tuple),
                    "Unspent outpoint should be in UTXO set, but NOT found: {:?}",
                    outpoint_tuple
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_recipient_spent_map() {
    let (blocks, _faucet, recipient, _db_dir, _zaino_db, db_reader) =
        load_vectors_db_and_reader().await;

    let (_recipient_txids, recipient_utxos, _recipient_balance) = recipient;
    let (_recipient_address, _txid, _output_index, recipient_script, _satoshis, _height) =
        recipient_utxos.first().unwrap().into_parts();
    let recipient_addr_script = AddrScript::from_script(&recipient_script.as_raw_bytes())
        .expect("faucet script must be standard P2PKH or P2SH");

    // collect faucet outpoints
    let mut recipient_outpoints = Vec::new();
    let mut recipient_ouptpoints_spent_status = Vec::new();
    for (_height, chain_block, _compact_block) in blocks.clone() {
        for tx in chain_block.transactions() {
            let txid = tx.txid();
            let outputs = tx.transparent().outputs();
            for (vout_idx, output) in outputs.iter().enumerate() {
                if output.script_hash() == recipient_addr_script.hash() {
                    let outpoint = Outpoint::new_from_be(txid, vout_idx as u32);

                    let spender = db_reader.get_outpoint_spender(outpoint).await.unwrap();

                    recipient_outpoints.push(outpoint);
                    recipient_ouptpoints_spent_status.push(spender);
                }
            }
        }
    }

    // collect faucet txids holding utxos
    let mut recipient_utxo_indexes = Vec::new();
    for utxo in recipient_utxos.iter() {
        let (_recipient_address, txid, output_index, _recipient_script, _satoshis, _height) =
            utxo.into_parts();
        recipient_utxo_indexes.push((txid.to_string(), output_index.index()));
    }

    // check full spent outpoints map
    let recipient_spent_map = db_reader
        .get_outpoint_spenders(recipient_outpoints.clone())
        .await
        .unwrap();
    assert_eq!(&recipient_ouptpoints_spent_status, &recipient_spent_map);

    for (outpoint, spender_option) in recipient_outpoints
        .iter()
        .zip(recipient_ouptpoints_spent_status.iter())
    {
        let outpoint_tuple = (
            crate::Hash::from(*outpoint.prev_txid()).to_string(),
            outpoint.prev_index(),
        );
        match spender_option {
            Some(spender_index) => {
                let spender_tx = blocks.iter().find_map(|(_h, chain_block, _cb)| {
                    chain_block.transactions().iter().find(|tx| {
                        let (block_height, tx_idx) =
                            (spender_index.block_index(), spender_index.tx_index());
                        chain_block.index().height() == Some(Height(block_height))
                            && tx.index() == tx_idx as u64
                    })
                });
                assert!(
                    spender_tx.is_some(),
                    "Spender transaction not found in blocks!"
                );

                let spender_tx = spender_tx.unwrap();
                let matches = spender_tx.transparent().inputs().iter().any(|input| {
                    input.prevout_txid() == outpoint.prev_txid()
                        && input.prevout_index() == outpoint.prev_index()
                });
                assert!(
                    matches,
                    "Spender transaction does not actually spend the outpoint: {:?}",
                    outpoint
                );

                assert!(
                    !recipient_utxo_indexes.contains(&outpoint_tuple),
                    "Spent outpoint should NOT be in UTXO set, but found: {:?}",
                    outpoint_tuple
                );
            }
            None => {
                assert!(
                    recipient_utxo_indexes.contains(&outpoint_tuple),
                    "Unspent outpoint should be in UTXO set, but NOT found: {:?}",
                    outpoint_tuple
                );
            }
        }
    }
}
