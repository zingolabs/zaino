//! Probe: time `z_gettreestate` (sync RPC #2) across a range of heights.
//!
//! This mimics the per-block commitment-tree fetch in
//! [`ZainoDB::sync_to_height`] — the exact
//! `JsonRpSeeConnector::get_treestate` + `read_commitment_tree` path the
//! fetch backend runs — *without* performing a full sync. It exists to
//! measure the validator-side cost of serving the Sapling/Orchard treestate
//! across the Orchard (NU5) activation boundary in minutes, instead of waiting
//! out a multi-day genesis-to-NU5 sync.
//!
//! `z_gettreestate` is independent per height (each call asks the validator for
//! the treestate as of that block), so a serial, increasing-height loop against
//! a fully-synced validator reproduces the sync's RPC-#2 pattern faithfully.
//! It also reuses the same `JsonRpSeeConnector` the sync uses — including its
//! built-in 5s request timeout, so a call that would exceed it errors here just
//! as it would inside `sync_to_height` (a timeout is itself a finding).
//!
//! The sync passes a block *hash*; this probe passes a *height* string, which
//! `z_gettreestate` resolves to the same block — the validator-side work being
//! measured is identical.
//!
//! ## Usage
//!
//! ```text
//! # cookie auth (zebra), consecutive run across NU5 (the "step" control):
//! RPC_COOKIE=/path/to/.cookie \
//!   cargo run --release -p zaino-state --example treestate_probe -- \
//!   127.0.0.1:8232 1687000:1687200:1 > step.csv
//!
//! # basic auth, sampled macro sweep through the sandblast window:
//! RPC_USER=u RPC_PASS=p \
//!   cargo run --release -p zaino-state --example treestate_probe -- \
//!   127.0.0.1:8232 1000000 1500000 1687104 1700000 1720000 1725000 > sweep.csv
//! ```
//!
//! Output (stdout, CSV): `height,rpc_seconds,parse_seconds,sapling_size,orchard_size`.
//! A blank `parse_seconds`/sizes row means the RPC errored (e.g. exceeded the
//! 5s timeout); the reason is logged to stderr.

use std::path::PathBuf;
use std::time::Instant;

use incrementalmerkletree::frontier::CommitmentTree;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_fetch::jsonrpsee::response::GetTreestateResponse;
use zcash_primitives::merkle_tree::read_commitment_tree;

type ProbeError = Box<dyn std::error::Error>;

/// Expands CLI height tokens into a flat, ordered list. Each token is either a
/// plain height (`1687104`) or an inclusive `START:END[:STEP]` range.
fn parse_heights(tokens: &[String]) -> Result<Vec<u32>, ProbeError> {
    let mut heights = Vec::new();
    for token in tokens {
        if token.contains(':') {
            let parts: Vec<&str> = token.split(':').collect();
            let start: u32 = parts[0].parse()?;
            let end: u32 = parts
                .get(1)
                .ok_or("range needs START:END[:STEP]")?
                .parse()?;
            let step: u32 = parts.get(2).map_or(Ok(1), |s| s.parse())?.max(1);
            let mut height = start;
            while height <= end {
                heights.push(height);
                height += step;
            }
        } else {
            heights.push(token.parse()?);
        }
    }
    Ok(heights)
}

#[tokio::main]
async fn main() -> Result<(), ProbeError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: treestate_probe <rpc_addr host:port> <height|START:END:STEP>...\n\
             auth via env: RPC_COOKIE=<path>  or  RPC_USER=<u> RPC_PASS=<p>"
        );
        std::process::exit(2);
    }
    let rpc_addr = args[0].clone();
    let heights = parse_heights(&args[1..])?;

    let cookie = std::env::var("RPC_COOKIE").ok().map(PathBuf::from);
    let user = std::env::var("RPC_USER").unwrap_or_else(|_| "__cookie__".into());
    let pass = std::env::var("RPC_PASS").unwrap_or_else(|_| "__cookie__".into());

    eprintln!(
        "probing {} heights against {rpc_addr} ({} auth)",
        heights.len(),
        if cookie.is_some() { "cookie" } else { "basic" }
    );

    let connector = JsonRpSeeConnector::new_from_config_parts(&rpc_addr, user, pass, cookie)
        .await
        .map_err(|e| format!("failed to build connector: {e}"))?;

    println!("height,rpc_seconds,parse_seconds,sapling_size,orchard_size");
    for height in heights {
        // RPC #2: the validator-side cost we are isolating.
        let rpc_start = Instant::now();
        let response = match connector.get_treestate(height.to_string()).await {
            Ok(response) => response,
            Err(e) => {
                let rpc_secs = rpc_start.elapsed().as_secs_f64();
                println!("{height},{rpc_secs:.6},,,");
                eprintln!("height {height}: get_treestate failed after {rpc_secs:.3}s: {e}");
                continue;
            }
        };
        let rpc_secs = rpc_start.elapsed().as_secs_f64();

        // The exact parse the sync performs: deserialize each pool's frontier
        // and compute its root. Timed separately to confirm it is negligible
        // next to the RPC (the CPU≈0 expectation).
        let parse_start = Instant::now();
        let GetTreestateResponse {
            sapling, orchard, ..
        } = response;
        let sapling_tree = sapling
            .map_or_else(
                || Some(Ok(CommitmentTree::<sapling_crypto::Node, 32>::empty())),
                |t| {
                    t.commitments().final_state().as_ref().map(|final_state| {
                        read_commitment_tree::<sapling_crypto::Node, _, 32>(final_state.as_slice())
                    })
                },
            )
            .transpose()?;
        let orchard_tree = orchard
            .map_or_else(
                || {
                    Some(Ok(
                        CommitmentTree::<zebra_chain::orchard::tree::Node, 32>::empty(),
                    ))
                },
                |t| {
                    t.commitments().final_state().as_ref().map(|final_state| {
                        read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(
                            final_state.as_slice(),
                        )
                    })
                },
            )
            .transpose()?;
        // `.root()` is the same work the sync does after parsing; call it so the
        // timing reflects the full per-block parse, not just deserialization.
        let sapling_size = sapling_tree.as_ref().map_or(0, |tree| {
            let _ = tree.root();
            tree.size()
        });
        let orchard_size = orchard_tree.as_ref().map_or(0, |tree| {
            let _ = tree.root();
            tree.size()
        });
        let parse_secs = parse_start.elapsed().as_secs_f64();

        println!("{height},{rpc_secs:.6},{parse_secs:.6},{sapling_size},{orchard_size}");
    }
    Ok(())
}
