//! Lightclient creation and management.

use std::path::PathBuf;
use tempfile::TempDir;
use testvectors::seeds;
use zingolib::{
    config::RegtestNetwork, lightclient::LightClient, testutils::scenarios::setup::ClientBuilder,
};

/// Holds zingo lightclients along with their TempDir for wallet-2-validator tests.
pub struct Clients {
    /// Lightclient TempDir location.
    pub lightclient_dir: TempDir,
    /// Faucet (zingolib lightclient).
    ///
    /// Mining rewards are received by this client for use in tests.
    pub faucet: LightClient,
    /// Recipient (zingolib lightclient).
    pub recipient: LightClient,
}

impl Clients {
    /// Returns the zcash address of the faucet.
    pub async fn get_faucet_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.faucet, pool)
    }

    /// Returns the zcash address of the recipient.
    pub async fn get_recipient_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.recipient, pool)
    }

    /// Launch lightclients for the given gRPC port.
    pub async fn launch(zaino_grpc_port: u16) -> Result<Self, std::io::Error> {
        let lightclient_dir = tempfile::tempdir()?;

        let (faucet, recipient) = build_lightclients(
            lightclient_dir.path().to_path_buf(),
            zaino_grpc_port,
        )
        .await;

        Ok(Self {
            lightclient_dir,
            faucet,
            recipient,
        })
    }
}

fn make_uri(indexer_port: u16) -> http::Uri {
    format!("http://127.0.0.1:{indexer_port}")
        .try_into()
        .unwrap()
}

// NOTE: this should be migrated to zingolib when LocalNet replaces regtest manager in zingolib::testutils
/// Builds faucet (miner) and recipient lightclients for local network integration testing
async fn build_lightclients(
    lightclient_dir: PathBuf,
    indexer_port: u16,
) -> (LightClient, LightClient) {
    let mut client_builder = ClientBuilder::new(make_uri(indexer_port), lightclient_dir);
    let faucet = client_builder.build_faucet(true, RegtestNetwork::all_upgrades_active());
    let recipient = client_builder.build_client(
        seeds::HOSPITAL_MUSEUM_SEED.to_string(),
        1,
        true,
        RegtestNetwork::all_upgrades_active(),
    );

    (faucet, recipient)
}