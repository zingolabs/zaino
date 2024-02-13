// nproxy.rs [bin]
// use: nym-proxy - recieves Grpc calls, serializes and sends calls over nym mixnet, returns Grpc response to zingolib
//

use std::thread;
use std::time::Duration;
use zingo_proxy::nproxy::spawn_server;

#[tokio::main]
async fn main() {
    nym_bin_common::logging::setup_logging();
    let server_port = 7070;
    spawn_server(server_port, 8080, 8080).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
