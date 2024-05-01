//! Zingo-Proxy daemon

use std::process;

use zingoproxylib::proxy::spawn_proxy;

extern crate ctrlc;

#[tokio::main]
async fn main() {
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    #[cfg(feature = "nym")]
    {
        nym_bin_common::logging::setup_logging();
    }

    let (_handles, _notify, _nym_address) = spawn_proxy(&8080, &9067, &18232).await;
}
