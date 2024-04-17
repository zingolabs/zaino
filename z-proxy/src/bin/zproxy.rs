// zproxy.rs [bin]
// use: zingo-proxy - recieves Grpc calls, passes Grpc calls to lightwalletd/zebrad, returns Grpc response.
//

use std::thread;
use std::time::Duration;

use z_proxy_lib::zproxy::spawn_server;

#[tokio::main]
async fn main() {
    let server_port = 8080;
    spawn_server(server_port, 9067, 18232).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
