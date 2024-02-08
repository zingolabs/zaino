use http::Uri;
use std::thread;
use std::time::Duration;
use zingo_proxy::proxy::{spawn_server, ProxyServer};

#[tokio::main]
async fn main() {
    let server_port = 7070;
    let server_handle = spawn_server(server_port, 8080, 8080).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
