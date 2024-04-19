//! zingo-nym daemon

use nymproxylib::proxy::spawn_server;
use nymproxylib::server::nym_serve;
use std::time::Duration;
use std::{process, thread};
use zingo_rpc::nym::utils::nym_spawn;
extern crate ctrlc;

#[tokio::main]
async fn main() {
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    #[cfg(not(feature = "nym_wallet"))]
    {
        let path = "/tmp/nym_server";
        let mut server = nym_spawn(path).await;
        let our_address = server.nym_address();
        println!("\nnserver - nym address: {our_address}");
        nym_serve(&mut server).await;
    }

    #[cfg(feature = "nym_wallet")]
    {
        nym_bin_common::logging::setup_logging();
        let server_port = 8080;
        spawn_server(server_port, 9067, 18232).await;
        loop {
            thread::sleep(Duration::from_secs(10));
        }
    }
}
