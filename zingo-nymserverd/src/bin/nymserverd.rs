//! zingo-nym daemon

use nymserverlib::server::nym_serve;
use std::process;
use zingo_rpc::nym::utils::nym_spawn;
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
