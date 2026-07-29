//! US-1.3 correctness: the address unspent-set merge across the finalised/recent
//! boundary. A finalised UTXO spent *inside the recent window* must be dropped
//! from a snapshot's unspent set (and a recent still-unspent UTXO added) — the
//! set algebra `resolve::merge_unspent` performs, driven here end-to-end through
//! the runtime so the wiring (which spend facet it consults) is covered too.

#[path = "support/mocks.rs"]
mod mocks;

use std::collections::HashSet;

use zaino_core::{Outpoint, Script, TransactionHash, TransparentAddress, Utxo, Zatoshis};
use zaino_runtime::{RuntimeBuilder, RuntimeConfig};
use zaino_service::AddressRead;

use mocks::{block_id, h, Calls, MockFs, MockNfs, MockNfsSnap, MockSource};

fn utxo(tag: u8) -> Utxo {
    Utxo {
        address: TransparentAddress::new("t1example".to_string()),
        txid: TransactionHash::from([tag; 32]),
        output_index: 0,
        script: Script::new(Vec::new()),
        satoshis: Zatoshis::new(1000).expect("valid amount"),
        height: h(50),
    }
}

fn outpoint(u: &Utxo) -> Outpoint {
    Outpoint {
        txid: u.txid,
        index: u.output_index,
    }
}

#[tokio::test]
async fn address_unspent_drops_finalised_outpoints_spent_in_the_window() {
    let calls = Calls::default();
    let a = utxo(0xA1); // finalised, spent within the recent window
    let b = utxo(0xB2); // finalised, still unspent
    let c = utxo(0xC3); // created within the window, unspent

    let fs = MockFs {
        watermark: h(100),
        calls: calls.clone(),
        finalised_utxos: vec![a.clone(), b.clone()],
    };
    let nfs = MockNfs {
        ready: true,
        finalised: h(100),
        snap: MockNfsSnap {
            tip: block_id(150, 0xAA),
            range: (h(101), h(150)),
            calls: calls.clone(),
            recent_utxos: vec![c.clone()],
            recent_spends: vec![outpoint(&a)],
        },
    };
    let source = MockSource {
        calls: calls.clone(),
    };

    let runtime = RuntimeBuilder::new()
        .config(RuntimeConfig {
            passthrough_enabled: true,
        })
        .assemble(fs, nfs, source)
        .serving_address_history()
        .finish()
        .await
        .expect("assemble");

    let snap = runtime.snapshot();
    let addr = TransparentAddress::new("t1example".to_string());
    let unspent = snap.unspent_outpoints(&addr).await.expect("merge ok");

    let got: HashSet<Outpoint> = unspent.iter().map(outpoint).collect();
    let want: HashSet<Outpoint> = [outpoint(&b), outpoint(&c)].into_iter().collect();
    assert_eq!(
        got, want,
        "A (spent in the recent window) must be dropped; B kept; C added"
    );
}
