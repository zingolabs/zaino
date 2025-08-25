//! Port allocation and network configuration.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

/// Network ports allocated for test services.
#[derive(Debug, Clone)]
pub struct TestPorts {
    /// Validator JSON-RPC port.
    pub validator_rpc: SocketAddr,
    /// Validator gRPC port (zebrd only).
    pub validator_grpc: SocketAddr,
    /// Zaino gRPC port.
    pub zaino_grpc: Option<SocketAddr>,
    /// Zaino JSON-RPC port.
    pub zaino_json: Option<SocketAddr>,
    /// Data directory for services.
    pub data_dir: PathBuf,
    /// Zaino database path.
    pub zaino_db: PathBuf,
    /// Zebra database path.
    pub zebra_db: PathBuf,
}

impl TestPorts {
    /// Allocate network ports for test services.
    pub async fn allocate() -> Result<Self, std::io::Error> {
        let validator_rpc_port = portpicker::pick_unused_port()
            .ok_or_else(|| std::io::Error::other("No ports free for validator RPC"))?;
        let validator_grpc_port = portpicker::pick_unused_port()
            .ok_or_else(|| std::io::Error::other("No ports free for validator gRPC"))?;

        let validator_rpc = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), validator_rpc_port);
        let validator_grpc = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), validator_grpc_port);

        let data_dir = tempfile::tempdir()?.keep();
        let zaino_db = data_dir.join("zaino");
        let zebra_db = data_dir.clone();

        Ok(Self {
            validator_rpc,
            validator_grpc,
            zaino_grpc: None,
            zaino_json: None,
            data_dir,
            zaino_db,
            zebra_db,
        })
    }

    /// Add zaino service ports.
    pub fn with_zaino_ports(&mut self) -> Result<(), std::io::Error> {
        let zaino_grpc_port = portpicker::pick_unused_port()
            .ok_or_else(|| std::io::Error::other("No ports free for zaino gRPC"))?;
        let zaino_json_port = portpicker::pick_unused_port()
            .ok_or_else(|| std::io::Error::other("No ports free for zaino JSON"))?;

        self.zaino_grpc = Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            zaino_grpc_port,
        ));
        self.zaino_json = Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            zaino_json_port,
        ));

        Ok(())
    }
}
