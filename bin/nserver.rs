// nserver.rs [bin]
// use: nym-server - receives serialized Grpc calls over nym mixnet, passes Grpc calls to lightwalletd/zebrad [currently zproxy], returns serialised Grpc response over nym mixnet.
//

use http::Uri;
use std::thread;
use std::time::Duration;
use zingo_proxy::nserver::tcp_listener;

#[tokio::main]
async fn main() {
    if let Err(e) = tcp_listener("127.0.0.1:9090").await {
        eprintln!("Failed to start TCP listener: {}", e);
    }
}
