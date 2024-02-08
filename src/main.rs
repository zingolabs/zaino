use http::Uri;
use proxy::{spawn_server, ProxyServer};
use std::thread;
use std::time::Duration;
mod proxy;

#[tokio::main]
async fn main() {
    let server_port = 8080;
    let server_handle = spawn_server(server_port, 9067, 18232).await;
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
