//use http::Uri;
//use proxy::{spawn_server, ProxyServer};

//use proxy::spawn_server;

//use zingoproxy;

// #[tokio::main]
// async fn main() {
//     let server_port = pick_unused_port().unwrap();
//     let server_handle = spawn_server(server_port, 9067, 18232).await;
//     sleep(Duration::from_secs(3)).await;
//     let proxy_uri = Uri::builder()
//         .scheme("http")
//         .authority(format!("localhost:{server_port}"))
//         .path_and_query("")
//         .build()
//         .unwrap();
// }
