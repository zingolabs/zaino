// nserver.rs [bin]
// use: nym-server - receives serialized Grpc calls over nym mixnet, passes Grpc calls to lightwalletd/zebrad [currently zproxy], returns serialised Grpc response over nym mixnet.
//

use http::Uri;
use std::time::Duration;
use std::{process, thread};
use zingo_proxy::nserver::{nym_serve, tcp_listener};
use zingo_proxy::nym_utils::{nym_close, nym_spawn};
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
