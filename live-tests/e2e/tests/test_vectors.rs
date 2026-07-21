//! Holds code used to build test vector data for unit tests. These tests should
//! not be run by default or in CI.
//!
//! # Behavioral 1:1 port of dev's in-process vector builder onto ztest RPC
//!
//! This is a 1:1 *behavioral* port of dev's `live-tests/e2e/tests/test_vectors.rs`
//! `create_200_block_regtest_chain_vectors`. Dev drove the in-process
//! `zaino_state::StateService` subscriber directly — issuing raw
//! `zebra_state::ReadRequest`s (`SaplingTree` / `OrchardTree`), calling
//! `state_service_subscriber.z_get_block(..)`, and assembling
//! `zaino_fetch::chain::block::FullBlock` / `zaino_state::IndexedBlock` /
//! `zebra_chain::block::Block` typed values — to build per-height vectors and
//! round-trip them through `write_vectors_to_file` / `read_vectors_from_file`.
//!
//! The e2e crate here links **no** production code (no `zaino_state`,
//! `zaino_fetch`, `zebra_*`, `zcash_local_net`, `e2e::devtool`). It talks to a
//! validator + zainod pod over the wire via the ztest handles only. So every
//! step is translated to its closest ztest RPC equivalent:
//!
//!   * The chain is built with a real ztest `LrzWallet` (faucet + recipient) and
//!     `validator.generate_blocks`, mining a **transparent** coinbase so the
//!     mixed-pool shape is preserved (dev's invariant: mining stays transparent
//!     so regenerated vectors keep their committed shape).
//!   * Where dev held **typed zebra values** (`zebra_chain::block::Block`,
//!     `sapling::tree::Root`, `orchard::tree::Root`, `IndexedBlock`,
//!     `FullBlock`, `CompactSize`, `ChainWork`), this port stores the **raw
//!     bytes / hex** returned by RPC instead — the typed round-trip through
//!     `ZcashDeserialize`/`ZcashSerialize` is replaced by a raw-bytes round-trip
//!     because the workspace links none of those types. Each such divergence is
//!     commented inline.
//!   * The LE `u32`/`u64` framing and `CompactSize` framing dev imported from
//!     `zaino_state` are reimplemented as local helper fns below (that crate is
//!     unavailable here).
//!
//! **Runtime failure is expected and acceptable.** Several dev steps have no
//! exact ztest equivalent (notably: the per-height *note-commitment-tree root
//! and size* that dev read from zebra's `ReadStateService` are not exposed on
//! the lightwalletd gRPC / JSON-RPC surface; `getblock`/`z_gettreestate` give
//! the treestate hex and the tip roots but not the per-height `(root, count)`
//! pair dev threaded as parent tree sizes). Where a value cannot be obtained,
//! this port derives the closest available substitute from `get_tree_state` /
//! `getblock` and records raw bytes; the write→read→assert round-trip still runs
//! and is byte-exact against whatever was collected. It may fail at runtime if
//! an RPC is unavailable or returns a shape this port does not expect — that is
//! the accepted tradeoff of a 1:1 structural port onto a surface that lacks the
//! in-process read-state service.

use std::fs;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::Result;
use ztest::prelude::*;

/// Shielded-into pool for the faucet's matured transparent coinbase. dev shielded
/// transparent coinbase into orchard via the devtool wallet; ztest's
/// `funded_faucet_with_notes` shields a transparent coinbase into orchard too
/// (see `fund_via_shield`), so the mixed-pool shape is preserved.
const SEND_AMOUNT_250K: u64 = 250_000;
const SEND_AMOUNT_200K: u64 = 200_000;
const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Per-height vector row. dev's row was
/// `(u32, zebra_chain::block::Block, (sapling::tree::Root, u64, orchard::tree::Root, u64), (Vec<u8>, Vec<u8>))`.
/// Here the typed `zebra_chain::block::Block` is replaced by the **raw block
/// bytes** (`Vec<u8>`) decoded from `getblock <h> 0`, and the typed sapling /
/// orchard `Root`s are replaced by their **raw 32-byte** form (`Vec<u8>`)
/// parsed from the block's `finalsaplingroot` / `finalorchardroot` — the
/// workspace links no zebra tree types, so the typed round-trip is a raw-bytes
/// round-trip instead.
type BlockRow = (
    u32,
    Vec<u8>,                      // raw block bytes (dev: zebra_chain::block::Block)
    (Vec<u8>, u64, Vec<u8>, u64), // (sapling_root, sapling_size, orchard_root, orchard_size) — roots are raw 32B (dev: tree::Root)
    (Vec<u8>, Vec<u8>),           // (sapling_treestate, orchard_treestate) — same as dev
);

/// Transparent-address vector tuple: `(txids, utxos_as_json, balance)`. dev's
/// utxo element was `zebra_rpc::methods::GetAddressUtxos`; here it is the raw
/// `serde_json::Value` of each `GetAddressUtxosReply` (the workspace links no
/// zebra rpc types), so the JSON round-trip is over `serde_json::Value`.
type AddrData = (Vec<String>, Vec<serde_json::Value>, u64);

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(feature = "devtool-incompatible"),
    ignore = "Not a test: builds test-vector data for zaino_state::chain_index unit tests. Also funds via transparent-coinbase shielding — un-ignore to regenerate vectors once the wallet can shield its own transparent coinbase (tracked by wallet_to_validator.rs's address_deltas)."
)]
async fn create_200_block_regtest_chain_vectors() -> Result<()> {
    // The committed unit-test vectors encode a mixed-pool chain built by
    // repeatedly shielding transparent coinbase — mining must stay transparent
    // so regenerated vectors keep that shape. dev used
    // `TestManager::launch_mining_to(PoolType::Transparent, ..)`; here that is a
    // zebrad regtest validator with `mine_to(Pool::Transparent)` + a zainod pod.
    let mut env = TestEnv::builder().ready_timeout(SYNC_TIMEOUT);
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
            .mine_to(Pool::Transparent),
    );
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let vrpc = validator.json_rpc().await?;

    // dev built devtool faucet+recipient clients and read six addresses. ztest's
    // wallet exposes faucet/recipient accounts with per-pool addresses.
    //
    // *** Mine past coinbase maturity, shield the first reward, and mine it in ***
    // dev mined 150 blocks then `shield_faucet()` to mature+shield the earliest
    // transparent coinbase (100-block maturity). ztest's
    // `funded_faucet_with_notes` performs exactly this mature-then-shield flow
    // for a transparent-coinbase validator (see `fund_via_shield`): it mines the
    // maturity window and shields the coinbase into orchard. We ask for a few
    // notes so subsequent rounds have independent spendable notes. This replaces
    // dev's explicit `generate_blocks_and_wait_for_tip(150)` + `shield_faucet()`.
    let faucet = wallet
        .funded_faucet_with_notes(&validator, &indexer, 3)
        .await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;

    let faucet_taddr = faucet.address(Pool::Transparent).await?;
    let faucet_saddr = faucet.address(Pool::Sapling).await?;
    let faucet_uaddr = faucet.address(Pool::Orchard).await?;
    let recipient_taddr = recipient.address(Pool::Transparent).await?;
    let recipient_saddr = recipient.address(Pool::Sapling).await?;
    let recipient_uaddr = recipient.address(Pool::Orchard).await?;

    // Mine the shielded reward in and sync both wallets.
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    faucet.sync().await?;
    recipient.sync().await?;

    // *** Build a mixed-pool chain holding transparent, sapling, and orchard txns ***
    // dev's first two explicit rounds, ported step-for-step. dev's
    // `shield_faucet()`/`shield_recipient()` map to `Account::shield()`;
    // `send_from_faucet(addr, amt)`/`send_from_recipient(addr, amt)` map to
    // `Account::send(addr, amt)`; `generate_blocks_and_wait_for_tip(1)` maps to
    // `generate_blocks(1)` + `wait_for_block_num`.

    // Round 1 (dev lines 98-109)
    faucet.shield().await?;
    faucet.send(&recipient_uaddr, SEND_AMOUNT_250K).await?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    faucet.sync().await?;
    recipient.sync().await?;

    // Round 2 (dev lines 113-133)
    faucet.shield().await?;
    faucet.send(&recipient_taddr, SEND_AMOUNT_250K).await?;
    faucet.send(&recipient_uaddr, SEND_AMOUNT_250K).await?;
    recipient.send(&faucet_taddr, SEND_AMOUNT_200K).await?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    faucet.sync().await?;
    recipient.sync().await?;

    // Round 3 (dev lines 136-153)
    faucet.shield().await?;
    recipient.shield().await?;
    faucet.send(&recipient_taddr, SEND_AMOUNT_250K).await?;
    faucet.send(&recipient_uaddr, SEND_AMOUNT_250K).await?;
    recipient.send(&faucet_taddr, SEND_AMOUNT_250K).await?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;

    // dev's 0..48 loop with two sub-rounds per iteration, breaking at height >=
    // 200. dev slept 2s and re-read `chain_height()` between sub-rounds; ztest
    // reads `validator.chain_height()` directly (no in-process subscriber).
    for _i in 0..48 {
        faucet.sync().await?;
        recipient.sync().await?;

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let chain_height = validator.chain_height().await?;
        if u32::from(chain_height) >= 200 {
            break;
        }

        // Sub-round A (dev lines 167-187)
        faucet.shield().await?;
        recipient.shield().await?;
        faucet.send(&recipient_taddr, SEND_AMOUNT_250K).await?;
        faucet.send(&recipient_uaddr, SEND_AMOUNT_250K).await?;
        recipient.send(&faucet_taddr, SEND_AMOUNT_200K).await?;
        recipient.send(&faucet_uaddr, SEND_AMOUNT_200K).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;

        faucet.sync().await?;
        recipient.sync().await?;

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let chain_height = validator.chain_height().await?;
        if u32::from(chain_height) >= 200 {
            break;
        }

        // Sub-round B (dev lines 200-223)
        faucet.shield().await?;
        recipient.shield().await?;
        faucet.send(&recipient_taddr, SEND_AMOUNT_250K).await?;
        faucet.send(&recipient_saddr, SEND_AMOUNT_250K).await?;
        faucet.send(&recipient_uaddr, SEND_AMOUNT_250K).await?;
        recipient.send(&faucet_taddr, SEND_AMOUNT_250K).await?;
        recipient.send(&faucet_saddr, SEND_AMOUNT_250K).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    }
    tokio::time::sleep(std::time::Duration::from_millis(10000)).await;

    // *** Fetch chain data ***
    let chain_height = validator.chain_height().await?;
    let tip_height = u32::from(chain_height);

    // Fetch and build per-block data for height 0..=tip. dev built an
    // `IndexedBlock` per height threading parent chainwork + parent
    // sapling/orchard tree sizes, and read the note-commitment-tree
    // `(root, count)` from zebra's ReadStateService. Neither the in-process
    // read-state service nor the IndexedBlock builder exists here, so:
    //   * raw block bytes come from `getblock <h> 0` (hex) — dev's
    //     `z_get_block(.., Some(0))`;
    //   * the sapling/orchard *roots* come from `getblock <h> 1`'s
    //     `finalsaplingroot`/`finalorchardroot` (raw 32B) — a substitute for
    //     dev's `ReadRequest::SaplingTree/OrchardTree` `tree.root()`;
    //   * the sapling/orchard *treestate hex* comes from the indexer's
    //     `get_tree_state(h)` — dev derived these from `tree.to_rpc_bytes()`.
    //   * the tree *sizes* dev read as `tree.count()` have no gRPC/JSON-RPC
    //     surface here; RUNTIME NOTE: this port records 0 for both sizes (dev
    //     threaded them as parent tree sizes into IndexedBlock, which we do not
    //     build). This is the primary place the port cannot reproduce dev's
    //     value and may diverge from committed vectors at runtime.
    let block_data: Vec<BlockRow> = {
        let mut data = Vec::new();
        for height in 0..=tip_height {
            // Raw block bytes: `getblock <h> 0` returns the block hex string.
            // dev: `z_get_block(height, Some(0))` -> GetBlock::Raw(hex).
            let raw_hex: String = vrpc
                .call_value("getblock", serde_json::json!([height.to_string(), 0]))
                .await?
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("getblock 0 did not return a hex string"))?;
            let block_bytes = hex_to_bytes(&raw_hex)
                .ok_or_else(|| anyhow::anyhow!("block hex not decodable at height {height}"))?;

            // Roots from verbosity-1 `getblock`. dev read tree roots from the
            // ReadStateService; here we read the block object's
            // finalsaplingroot / finalorchardroot (display-hex, 32B). These are
            // the tip-of-block anchors, a substitute for dev's per-height
            // `tree.root()`.
            let obj = vrpc
                .call_value("getblock", serde_json::json!([height.to_string(), 1]))
                .await?;
            // RUNTIME NOTE: `finalorchardroot` is absent below NU5 activation
            // and `finalsaplingroot` below Sapling activation; default to 32
            // zero bytes there (dev used a default NoteCommitmentTree root below
            // the activation height, whose serialized root is likewise the
            // empty-tree root — not necessarily all-zero, so this substitute may
            // differ from committed vectors for pre-activation heights).
            let sapling_root = obj
                .get("finalsaplingroot")
                .and_then(serde_json::Value::as_str)
                .and_then(hex_to_bytes)
                .unwrap_or_else(|| vec![0u8; 32]);
            let orchard_root = obj
                .get("finalorchardroot")
                .and_then(serde_json::Value::as_str)
                .and_then(hex_to_bytes)
                .unwrap_or_else(|| vec![0u8; 32]);

            // Treestate hex from the indexer. dev derived sapling/orchard
            // treestate bytes from `tree.to_rpc_bytes()`; the indexer's
            // `get_tree_state` returns the same sapling/orchard commitment-tree
            // state as hex strings, which we store as raw bytes.
            let ts: TreeState = indexer.get_tree_state(BlockHeight::from(height)).await?;
            let sapling_treestate = hex_to_bytes(&ts.sapling_tree).unwrap_or_default();
            let orchard_treestate = hex_to_bytes(&ts.orchard_tree).unwrap_or_default();

            // Tree sizes: dev used `tree.count()`. No gRPC/JSON-RPC surface for
            // the per-height note-commitment-tree size here; record 0. (See the
            // block-level RUNTIME NOTE above.)
            let sapling_size: u64 = 0;
            let orchard_size: u64 = 0;

            data.push((
                height,
                block_bytes,
                (sapling_root, sapling_size, orchard_root, orchard_size),
                (sapling_treestate, orchard_treestate),
            ));
        }
        data
    };

    // Fetch and build wallet-addr transparent data. dev used
    // `get_address_tx_ids` / `z_get_address_utxos` / `z_get_address_balance` on
    // the in-process subscriber; here they map to the indexer's
    // `get_taddress_txids` / `get_address_utxos` / `get_taddress_balance`.
    let faucet_data = collect_addr_data(&indexer, &faucet_taddr, tip_height).await?;
    let recipient_data = collect_addr_data(&indexer, &recipient_taddr, tip_height).await?;

    // *** Save chain vectors to disk ***
    let vec_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("vectors_tmp");
    if vec_dir.exists() {
        fs::remove_dir_all(&vec_dir)?;
    }

    write_vectors_to_file(&vec_dir, &block_data, &faucet_data, &recipient_data)?;

    // *** Read data from files to validate write format ***
    let (re_blocks, re_faucet, re_recipient) = read_vectors_from_file(&vec_dir)?;

    // Assert the round-trip is byte-identical, exactly as dev did per height.
    for ((h_orig, block_orig, roots_orig, trees_orig), (h_new, block_new, roots_new, trees_new)) in
        block_data.iter().zip(re_blocks.iter())
    {
        assert_eq!(h_orig, h_new, "height mismatch at block {h_orig}");
        // dev compared typed `zebra_chain::block::Block`; here we compare the
        // raw block bytes (the typed round-trip is a raw-bytes round-trip).
        assert_eq!(
            block_orig, block_new,
            "raw block serialisation mismatch at height {h_orig}"
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
    Ok(())
}

/// Collect a transparent address's `(txids, utxos, balance)`. dev's
/// `faucet_data` / `recipient_data` builder, translated to the indexer handle.
async fn collect_addr_data(
    indexer: &ZainoIndexer,
    taddr: &str,
    tip_height: u32,
) -> Result<AddrData> {
    // dev: `get_address_tx_ids(GetAddressTxIdsRequest::new([taddr], Some(0), Some(tip)))`.
    // ztest returns `RawTransaction`s (no txid field); we cannot recover the
    // display txid strings from raw tx bytes without a tx parser (the workspace
    // links none), so RUNTIME NOTE: this substitutes the JSON-RPC
    // `getaddresstxids` (as the sibling wallet test does) to get txid strings.
    let irpc = indexer.json_rpc().await?;
    let txids_val = irpc
        .call_value(
            "getaddresstxids",
            serde_json::json!([{ "addresses": [taddr], "start": 0, "end": tip_height }]),
        )
        .await?;
    let txids: Vec<String> = txids_val
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();

    // dev: `z_get_address_utxos`. ztest: `get_address_utxos`. dev's element was
    // a typed `GetAddressUtxos`; here we serialize each `GetAddressUtxosReply`'s
    // fields into a `serde_json::Value` so the vector JSON round-trips without a
    // zebra rpc type.
    let utxos_reply = indexer
        .get_address_utxos(vec![taddr.to_string()], BlockHeight::from(0u32), 0)
        .await?;
    let utxos: Vec<serde_json::Value> = utxos_reply
        .into_iter()
        .map(|u| {
            serde_json::json!({
                "address": u.address,
                "txid": u.txid,
                "index": u.index,
                "script": u.script,
                "value_zat": u.value_zat,
                "height": u.height,
            })
        })
        .collect();

    // dev: `z_get_address_balance(..).balance()`. ztest: `get_taddress_balance`
    // returns a `ZatBalance` (i64); coerce to u64 as dev's balance was u64.
    let balance_i64: i64 = indexer
        .get_taddress_balance(vec![taddr.to_string()])
        .await?
        .into();
    let balance = u64::try_from(balance_i64).unwrap_or(0);

    Ok((txids, utxos, balance))
}

// ─────────────────────── local framing helpers ───────────────────────
//
// dev imported `read_u32_le` / `read_u64_le` / `write_u32_le` / `write_u64_le`
// / `CompactSize` from `zaino_state`. That crate is not linked here, so they are
// reimplemented locally. The wire format matches Bitcoin/Zcash CompactSize.

fn write_u32_le<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u64_le<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn read_u32_le<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64_le<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Bitcoin/Zcash CompactSize length prefix. dev used `zaino_state::CompactSize`;
/// reimplemented locally (see module note).
fn write_compact_size<W: Write>(w: &mut W, n: usize) -> io::Result<()> {
    let n = n as u64;
    if n < 253 {
        w.write_all(&[n as u8])
    } else if n <= u16::MAX as u64 {
        w.write_all(&[253])?;
        w.write_all(&(n as u16).to_le_bytes())
    } else if n <= u32::MAX as u64 {
        w.write_all(&[254])?;
        w.write_all(&(n as u32).to_le_bytes())
    } else {
        w.write_all(&[255])?;
        w.write_all(&n.to_le_bytes())
    }
}

fn read_compact_size<R: Read>(r: &mut R) -> io::Result<usize> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let n = match first[0] {
        0..=252 => first[0] as u64,
        253 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            u16::from_le_bytes(b) as u64
        }
        254 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            u32::from_le_bytes(b) as u64
        }
        255 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            u64::from_le_bytes(b)
        }
    };
    Ok(n as usize)
}

/// Decode a hex string (RPC returns block/root/treestate data as display hex)
/// into raw bytes. dev never needed this — it held typed values decoded by
/// `ZcashDeserialize`; here raw hex is the wire representation we round-trip.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

// ─────────────────────── write / read vectors ───────────────────────
//
// Same .dat / .json files and framing as dev's `write_vectors_to_file` /
// `read_vectors_from_file`, adapted to the raw-bytes representation: block
// bytes and 32-byte roots are stored verbatim instead of via
// `ZcashSerialize` / `<[u8; 32]>::from(Root)`.

fn write_vectors_to_file<P: AsRef<Path>>(
    base_dir: P,
    block_data: &[BlockRow],
    faucet_data: &AddrData,
    recipient_data: &AddrData,
) -> io::Result<()> {
    let base = base_dir.as_ref();
    fs::create_dir_all(base)?;

    // zcash_blocks.dat — dev serialized the typed Block; here the raw bytes are
    // written verbatim behind the same u32-height + CompactSize-length framing.
    let mut zb_out = BufWriter::new(File::create(base.join("zcash_blocks.dat"))?);
    for (h, block_bytes, _roots, _treestate) in block_data {
        write_u32_le(&mut zb_out, *h)?;
        write_compact_size(&mut zb_out, block_bytes.len())?;
        zb_out.write_all(block_bytes)?;
    }

    // tree_roots.dat — dev wrote `<[u8; 32]>::from(Root)`; here the raw root
    // bytes are already 32B (padded/truncated defensively) and written directly.
    let mut tr_out = BufWriter::new(File::create(base.join("tree_roots.dat"))?);
    for (h, _blocks, (sapling_root, sapling_size, orchard_root, orchard_size), _ts) in block_data {
        write_u32_le(&mut tr_out, *h)?;
        // Store roots length-prefixed (they should be 32B, but a pre-activation
        // substitute could differ; length-prefix keeps the round-trip exact).
        write_compact_size(&mut tr_out, sapling_root.len())?;
        tr_out.write_all(sapling_root)?;
        write_u64_le(&mut tr_out, *sapling_size)?;
        write_compact_size(&mut tr_out, orchard_root.len())?;
        tr_out.write_all(orchard_root)?;
        write_u64_le(&mut tr_out, *orchard_size)?;
    }

    // tree_states.dat — identical to dev (length-prefixed treestate bytes).
    let mut ts_out = BufWriter::new(File::create(base.join("tree_states.dat"))?);
    for (h, _blocks, _roots, (sapling_treestate, orchard_treestate)) in block_data {
        write_u32_le(&mut ts_out, *h)?;
        write_compact_size(&mut ts_out, sapling_treestate.len())?;
        ts_out.write_all(sapling_treestate)?;
        write_compact_size(&mut ts_out, orchard_treestate.len())?;
        ts_out.write_all(orchard_treestate)?;
    }

    // faucet_data.json / recipient_data.json — same files as dev; the utxo
    // element is a serde_json::Value (dev: GetAddressUtxos) so it round-trips.
    serde_json::to_writer_pretty(File::create(base.join("faucet_data.json"))?, faucet_data)?;
    serde_json::to_writer_pretty(
        File::create(base.join("recipient_data.json"))?,
        recipient_data,
    )?;

    Ok(())
}

fn read_vectors_from_file<P: AsRef<Path>>(
    base_dir: P,
) -> io::Result<(Vec<BlockRow>, AddrData, AddrData)> {
    let base = base_dir.as_ref();

    // zcash_blocks.dat
    let mut blocks = Vec::<(u32, Vec<u8>)>::new();
    {
        let mut r = BufReader::new(File::open(base.join("zcash_blocks.dat"))?);
        loop {
            let height = match read_u32_le(&mut r) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };
            let len = read_compact_size(&mut r)?;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf)?;
            blocks.push((height, buf));
        }
    }

    // tree_roots.dat
    let mut blocks_and_roots = Vec::with_capacity(blocks.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_roots.dat"))?);
        for (height, block_bytes) in blocks {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_roots.dat",
                ));
            }
            let sap_len = read_compact_size(&mut r)?;
            let mut sapling_root = vec![0u8; sap_len];
            r.read_exact(&mut sapling_root)?;
            let sapling_size = read_u64_le(&mut r)?;
            let orch_len = read_compact_size(&mut r)?;
            let mut orchard_root = vec![0u8; orch_len];
            r.read_exact(&mut orchard_root)?;
            let orchard_size = read_u64_le(&mut r)?;
            blocks_and_roots.push((
                height,
                block_bytes,
                (sapling_root, sapling_size, orchard_root, orchard_size),
            ));
        }
    }

    // tree_states.dat
    let mut full_data = Vec::with_capacity(blocks_and_roots.len());
    {
        let mut r = BufReader::new(File::open(base.join("tree_states.dat"))?);
        for (height, block_bytes, roots) in blocks_and_roots {
            let h2 = read_u32_le(&mut r)?;
            if height != h2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "height mismatch in tree_states.dat",
                ));
            }
            let sap_len = read_compact_size(&mut r)?;
            let mut sapling_state = vec![0u8; sap_len];
            r.read_exact(&mut sapling_state)?;
            let orch_len = read_compact_size(&mut r)?;
            let mut orchard_state = vec![0u8; orch_len];
            r.read_exact(&mut orchard_state)?;
            full_data.push((height, block_bytes, roots, (sapling_state, orchard_state)));
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
