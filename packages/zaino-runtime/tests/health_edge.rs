//! The health edge, end to end: a real HTTP probe answers from the live
//! runtime aggregate.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zaino_component::ComponentName;
use zaino_runtime::{
    HealthServer, OrchestraBuilder, ServeComponent, ValidatorComponent, ValidatorProbe,
};

struct Up;
impl ValidatorProbe for Up {
    async fn reachable(&self) -> bool {
        true
    }
}

/// Grab a likely-free port (the listener is dropped before we hand the address on).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    listener.local_addr().expect("addr").port()
}

/// Minimal HTTP GET; returns the status code.
async fn get_status(addr: SocketAddr, path: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).await.expect("connect health");
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: probe\r\n\r\n").as_bytes())
        .await
        .expect("write request");
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await.expect("read response");
    let response = String::from_utf8_lossy(&buf[..n]);
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code")
}

#[tokio::test]
async fn the_health_edge_reflects_the_runtime() {
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("valid addr");

    let builder = OrchestraBuilder::new();
    let signals = builder.signals();
    let validator = ValidatorComponent::connect(&Up).await.expect("connect");
    let health = ServeComponent::new(ComponentName("health"), HealthServer::new(addr, signals));

    let orchestra = builder
        .boot_observed(validator)
        .await
        .boot(health)
        .await
        .expect("boot health")
        .build();

    // Wait until the aggregate reports ready (validator + health both up).
    let mut sig = orchestra.signals();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if sig.borrow().ready {
                return;
            }
            if sig.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .expect("runtime became ready");

    assert_eq!(get_status(addr, "/livez").await, 200);
    assert_eq!(get_status(addr, "/readyz").await, 200);
    assert_eq!(get_status(addr, "/startupz").await, 200);
    assert_eq!(get_status(addr, "/nope").await, 404);

    orchestra.shutdown();
}
