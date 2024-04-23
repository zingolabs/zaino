//! Zingo-Proxy daemon

use std::time::Duration;
use std::{process, thread};

extern crate ctrlc;

use zingo_rpc::nym::utils::nym_spawn;
use zingoproxylib::{nym_server::nym_serve, server::spawn_server};

#[tokio::main]
async fn main() {
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    #[cfg(any(feature = "nym_wallet", feature = "nym_server"))]
    {
        nym_bin_common::logging::setup_logging();
    }

    #[cfg(any(not(feature = "nym_server"), feature = "nym_wallet"))]
    {
        let server_port = 8080;
        spawn_server(server_port, 9067, 18232).await;
        loop {
            thread::sleep(Duration::from_secs(10));
        }
    }

    #[cfg(all(feature = "nym_server", not(feature = "nym_wallet")))]
    {
        let path = "/tmp/nym_server";
        let mut server = nym_spawn(path).await;
        let our_address = server.nym_address();
        println!("\nnserver - nym address: {our_address}");
        nym_serve(&mut server).await;
    }
}
