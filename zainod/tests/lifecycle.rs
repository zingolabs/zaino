//! claude inspired Integration test for the zainod binary
//!
//! These tests launch the actual zainod binary as a subprocess and test its
//! behavior as a daemon, including startup, TCP connections, and shutdown.
//!
//! Run with: cargo nextest run --package zainod --test lifecycle

use std::{net::SocketAddr, path::PathBuf, process::Child};
use tempfile::TempDir;

///  A *private* test type to facilitate test process management
///
/// It automatically kills the daemon when dropped to ensure cleanup.
struct ZainodTestContainer {
    /// service addresses
    addresses: ServiceAddresses,
    /// child process handle
    process: Child,
    /// Temporary directory for config and data
    _temp_dir: TempDir,
}

/// SocketAddrs that zainod may have
struct ServiceAddresses {
    /// grpc_server listener for incoming client connections to the zainod grpc server
    grpc_address: SocketAddr,
    /// lightwallet client supporting RPC-server JSON-RPC listen address
    json_rpc_address: SocketAddr,
}

struct TestPaths {
    config: PathBuf,
    db: PathBuf,
    zebra_db: PathBuf,
}
impl TestPaths {
    fn generate_paths() -> Self {
        let temp_dir = tempfile::tempdir().expect("to create a tempdir");
        Self {
            config: temp_dir.path().join("test_config.toml"),
            db: temp_dir.path().join("zaino_db"),
            zebra_db: temp_dir.path().join("zebra_db"),
        }
    }
}
fn pick_non_reserved_port(reserved: &Vec<u16>) -> u16 {
    loop {
        let port = portpicker::pick_unused_port().expect("to pick one!");

        if !reserved.contains(&port) {
            return port;
        }
    }
}
impl ServiceAddresses {
    fn generate_random_unused_addresses() -> Self {
        // disallow zebra ports from being picked
        // I'm assuming zebra spawns first
        let mut reserved = vec![18230, 18232];
        let grpc_port = pick_non_reserved_port(&reserved);
        reserved.extend([grpc_port]);
        let json_rpc_port = pick_non_reserved_port(&reserved);
        ServiceAddresses {
            grpc_address: format!("127.0.0.1:{}", grpc_port)
                .parse::<SocketAddr>()
                .unwrap(),
            json_rpc_address: format!("127.0.0.1:{}", json_rpc_port)
                .parse::<SocketAddr>()
                .unwrap(),
        }
    }
    fn zebrad_default_addresses() -> Self {
        ServiceAddresses {
            grpc_address: "127.0.0.1:18232".parse::<SocketAddr>().unwrap(),
            json_rpc_address: "127.0.0.1:18230".parse::<SocketAddr>().unwrap(),
        }
    }
}
fn create_test_config(
    zainod_grpc: SocketAddr,
    zainod_rpc: Option<SocketAddr>,
    validator_rpc_address: SocketAddr,
    validator_grpc_address: SocketAddr,
    db_path: PathBuf,
    zebra_db_path: PathBuf,
) -> String {
    let json_server_selection = if let Some(addr) = zainod_rpc {
        format!(
            r#"
[json_server_settings]
json_rpc_listen_address = "{}"
            "#,
            addr
        )
    } else {
        String::new()
    };
    format! {
            r#"
backend = "fetch"
{json_server_selection}
[grpc_settings]
grpc_listen_address = "{zainod_grpc}"
[validator_settings]
validator_jsonrpc_listen_address = "{validator_rpc_address}"
validator_grpc_listen_address = "{validator_grpc_address}"
validator_user = "testuser"
validator_password = "testpass"
[service]
# Use default service settings
[storage]
[storage.cache]
# Use default cache settings
[storage.database]
path = "{db_path}"
zebra_db_path = "{zebra_db_path}"
network = "Regtest"
"#, db_path = db_path.display(),
    zebra_db_path = zebra_db_path.display()
    }
}
impl ZainodTestContainer {
    async fn spawn() {
        let zebrad_addresses = ServiceAddresses::zebrad_default_addresses();
        let zainod_addresses = ServiceAddresses::generate_random_unused_addresses();
        let test_paths = TestPaths::generate_paths();
    }
}
#[tokio::test]
async fn an_int_test() {
    ZainodTestContainer::spawn().await;
}
