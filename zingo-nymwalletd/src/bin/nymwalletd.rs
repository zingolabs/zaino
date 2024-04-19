//! zingo-nym daemon

use nymwalletlib::server::spawn_server;
use std::time::Duration;
use std::{process, thread};
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
