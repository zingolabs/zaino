//! nym-server daemon

use std::process;

use nymproxylib::{server::nym_serve, utils::nym_spawn};
extern crate ctrlc;

#[tokio::main]
async fn main() {
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");
    let path = "/tmp/nym_server";
    let mut server = nym_spawn(path).await;
    let our_address = server.nym_address();
    println!("\nnserver - nym address: {our_address}");
    nym_serve(&mut server).await;
}

// #[tokio::main]
// async fn main() {
//     if let Err(e) = tcp_listener("127.0.0.1:9090").await {
//         eprintln!("Failed to start TCP listener: {}", e);
//     }
// }
