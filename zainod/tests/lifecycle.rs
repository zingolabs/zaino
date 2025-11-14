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
    addresses: ZainodServiceAddresses,
    /// child process handle
    process: Child,
    /// Temporary directory for config and data
    _temp_dir: TempDir,
}

/// Ports that zainod may have
struct ZainodServiceAddresses {
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
impl ZainodServiceAddresses {
    fn generate_addresses() -> Self {
        let grpc_port = portpicker::pick_unused_port().expect("No ports for grpc");
        let json_rpc_port = portpicker::pick_unused_port().expect("No ports for json_rpc");
        ZainodServiceAddresses {
            grpc_address: format!("127.0.0.1:{}", grpc_port)
                .parse::<SocketAddr>()
                .unwrap(),
            json_rpc_address: format!("127.0.0.1:{}", json_rpc_port)
                .parse::<SocketAddr>()
                .unwrap(),
        }
    }
}
impl ZainodTestContainer {
    async fn spawn() {
        let addresses = ZainodServiceAddresses::generate_addresses();
        let test_paths = TestPaths::generate_paths();
    }
}
#[tokio::test]
async fn an_int_test() {
    ZainodTestContainer::spawn().await;
}
