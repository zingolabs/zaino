//! Zingo-Proxy daemon

use std::{
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use zingoproxylib::proxy::spawn_proxy;

extern crate ctrlc;

#[tokio::main]
async fn main() {
    let online = Arc::new(AtomicBool::new(true));
    let online_ctrlc = online.clone();
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        online_ctrlc.store(false, Ordering::SeqCst);
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    nym_bin_common::logging::setup_logging();

    let proxy_port: u16 = 8080;
    #[cfg(feature = "nym_poc")]
    {
        let proxy_port = 8088;
    }
    let lwd_port: u16 = 9067;
    let zcashd_port: u16 = 18232;

    let (_handles, _nym_address) =
        spawn_proxy(&proxy_port, &lwd_port, &zcashd_port, online.clone()).await;

    while online.load(Ordering::SeqCst) {}
}
