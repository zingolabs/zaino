//! Zingo-Indexer daemon

use std::{
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use zingoindexerlib::indexer::spawn_indexer;

#[tokio::main]
async fn main() {
    let online = Arc::new(AtomicBool::new(true));
    let online_ctrlc = online.clone();
    ctrlc::set_handler(move || {
        println!("@zingoindexerd: Received Ctrl+C, exiting.");
        online_ctrlc.store(false, Ordering::SeqCst);
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    nym_bin_common::logging::setup_logging();

    #[allow(unused_mut)]
    let mut indexer_port: u16 = 8080;
    #[cfg(feature = "nym_poc")]
    {
        indexer_port = 8088;
    }

    #[allow(unused_mut)]
    let mut lwd_port: u16 = 9067;
    #[cfg(feature = "nym_poc")]
    {
        lwd_port = 8080;
    }

    let zcashd_port: u16 = 18232;

    let (_handles, _nym_address) = spawn_indexer(
        &indexer_port,
        &lwd_port,
        &zcashd_port,
        "/tmp/nym_server",
        online.clone(),
    )
    .await;

    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    while online.load(Ordering::SeqCst) {
        interval.tick().await;
    }
}
