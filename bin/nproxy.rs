// nproxy.rs [bin]
// use: nym-proxy - recieves Grpc calls, serializes and sends calls over nym mixnet, returns Grpc response to zingolib
//

use std::time::Duration;
use std::{process, thread};
use zingo_proxy::nproxy::spawn_server;
extern crate ctrlc;

#[tokio::main]
async fn main() {
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");
    nym_bin_common::logging::setup_logging();
    let server_port = 8080;
    spawn_server(server_port, 9067, 18232).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
