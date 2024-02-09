// nserver.rs
// use: nym-server - receives serialized Grpc calls over nym mixnet, passes Grpc calls to lightwalletd/zebrad [currently zproxy], returns serialised Grpc response over nym mixnet.
//

use http::Uri;
use std::thread;
use std::time::Duration;
use zingo_proxy::nserver_utils::{spawn_server, ProxyServer};

#[tokio::main]
async fn main() {
    let server_port = 8080;
    let server_handle = spawn_server(server_port, 9067, 18232).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
