//! Utility functions for Zingo-Proxy Testing.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::io::Write;

static CTRL_C_ONCE: std::sync::Once = std::sync::Once::new();

/// Configuration data for Zingo-Proxy Tests.
pub struct TestManager {
    /// Temporary Directory for nym, zcashd and lightwalletd configuration and regtest data.
    pub temp_conf_dir: tempfile::TempDir,
    // std::path::PathBuf,
    /// Zingolib regtest manager.
    pub regtest_manager: zingo_testutils::regtest::RegtestManager,
    /// Zingolib regtest network.
    pub regtest_network: zingoconfig::RegtestNetwork,
    /// Zingo-Proxy gRPC listen port.
    pub proxy_port: u16,
    /// Zingo-Proxy Nym listen address.
    pub nym_addr: Option<String>,
    /// Zebrad/Zcashd JsonRpc listen port.
    pub zebrad_port: u16,
    /// Online status of Zingo-Proxy.
    pub online: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TestManager {
    /// Launches a zingo regtest manager and zingo-proxy, created TempDir for configuration and log files.
    pub async fn launch(
        online: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> (
        Self,
        zingo_testutils::regtest::ChildProcessHandler,
        Vec<tokio::task::JoinHandle<Result<(), tonic::transport::Error>>>,
    ) {
        let lwd_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_port = portpicker::pick_unused_port().expect("No ports free");
        let proxy_port = portpicker::pick_unused_port().expect("No ports free");

        let temp_conf_dir = create_temp_conf_files(lwd_port, zebrad_port).unwrap();
        let temp_conf_path = temp_conf_dir.path().to_path_buf();
        let nym_conf_path = temp_conf_path.join("nym");

        set_custom_drops(online.clone(), Some(temp_conf_path.clone()));

        let regtest_network = zingoconfig::RegtestNetwork::new(1, 1, 1, 1, 1, 1);

        let regtest_manager = zingo_testutils::regtest::RegtestManager::new(temp_conf_path.clone());
        let regtest_handler = regtest_manager
            .launch(true)
            .expect("Failed to start regtest services");

        let (proxy_handler, nym_addr) = zingoproxylib::proxy::spawn_proxy(
            &proxy_port,
            &lwd_port,
            &zebrad_port,
            nym_conf_path.to_str().unwrap(),
            online.clone(),
        )
        .await;

        (
            TestManager {
                temp_conf_dir,
                regtest_manager,
                regtest_network,
                proxy_port,
                nym_addr,
                zebrad_port,
                online,
            },
            regtest_handler,
            proxy_handler,
        )
    }

    /// Returns zingo-proxy listen address.
    pub fn get_proxy_uri(&self) -> http::Uri {
        http::Uri::builder()
            .scheme("http")
            .authority(format!("127.0.0.1:{0}", self.proxy_port))
            .path_and_query("")
            .build()
            .unwrap()
    }

    /// Returns zebrad listen address.
    pub async fn test_and_return_zebrad_uri(&self) -> http::Uri {
        zingo_rpc::jsonrpc::connector::test_node_and_return_uri(
            &self.zebrad_port,
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await
        .unwrap()
    }

    /// Builds aand returns Zingolib lightclient.
    pub async fn build_lightclient(&self) -> zingolib::lightclient::LightClient {
        let mut client_builder = zingo_testutils::scenarios::setup::ClientBuilder::new(
            self.get_proxy_uri(),
            self.temp_conf_dir.path().to_path_buf(),
        );
        client_builder
            .build_faucet(false, self.regtest_network)
            .await
    }
}

/// Closes test manager child processes, optionally cleans configuration and log files for test.
pub async fn drop_test_manager(
    temp_conf_path: Option<std::path::PathBuf>,
    child_process_handler: zingo_testutils::regtest::ChildProcessHandler,
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    online.store(false, std::sync::atomic::Ordering::SeqCst);
    drop(child_process_handler);

    let mut temp_wallet_path = temp_conf_path.clone().unwrap();
    if let Some(dir_name) = temp_wallet_path.file_name().and_then(|n| n.to_str()) {
        let new_dir_name = format!("{}_client_1", dir_name);
        temp_wallet_path.set_file_name(new_dir_name); // Update the directory name
    }

    if let Some(ref path) = temp_conf_path {
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!(
                "@zingoproxyd: Failed to delete temporary regtest configuration directory: {:?}.",
                e
            );
        }
    }
    if let Some(ref path) = Some(temp_wallet_path) {
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!(
                "@zingoproxyd: Failed to delete temporary directory: {:?}.",
                e
            );
        }
    }
}

fn set_custom_drops(
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
    temp_conf_path: Option<std::path::PathBuf>,
) {
    let online_panic = online.clone();
    let online_ctrlc = online.clone();
    let temp_conf_path_panic = temp_conf_path.clone();
    let temp_conf_path_ctrlc = temp_conf_path.clone();

    let mut temp_wallet_path = temp_conf_path.unwrap();
    if let Some(dir_name) = temp_wallet_path.file_name().and_then(|n| n.to_str()) {
        let new_dir_name = format!("{}_client_1", dir_name);
        temp_wallet_path.set_file_name(new_dir_name); // Update the directory name
    }
    let temp_wallet_path_panic = Some(temp_wallet_path.clone());
    let temp_wallet_path_ctrlc = Some(temp_wallet_path.clone());

    let default_panic_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        default_panic_hook(panic_info);
        online_panic.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(ref path) = temp_conf_path_panic {
            if let Err(e) = std::fs::remove_dir_all(path) {
                eprintln!(
                    "@zingoproxyd: Failed to delete temporary regtest config directory: {:?}.",
                    e
                );
            }
        }
        if let Some(ref path) = temp_wallet_path_panic {
            if let Err(e) = std::fs::remove_dir_all(path) {
                eprintln!(
                    "@zingoproxyd: Failed to delete temporary wallet directory: {:?}.",
                    e
                );
            }
        }
        // Assures tests fail on secondary thread panics.
        assert!(false);
        std::process::exit(0);
    }));

    CTRL_C_ONCE.call_once(|| {
        ctrlc::set_handler(move || {
            println!("@zingoproxyd: Received Ctrl+C, exiting.");
            online_ctrlc.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Some(ref path) = temp_conf_path_ctrlc {
                if let Err(e) = std::fs::remove_dir_all(path) {
                    eprintln!(
                        "@zingoproxyd: Failed to delete temporary regtest config directory: {:?}.",
                        e
                    );
                }
            }
            if let Some(ref path) = temp_wallet_path_ctrlc {
                if let Err(e) = std::fs::remove_dir_all(path) {
                    eprintln!(
                        "@zingoproxyd: Failed to delete temporary wallet directory: {:?}.",
                        e
                    );
                }
            }
            // Assures tests fail on ctrlc exit.
            assert!(false);
            std::process::exit(0);
        })
        .expect("Error setting Ctrl-C handler");
    })
}

fn write_lightwalletd_yml(
    dir: &std::path::Path,
    bind_addr_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = dir.join("lightwalletd.yml");
    let mut file = std::fs::File::create(file_path)?;
    writeln!(file, "grpc-bind-addr: 127.0.0.1:{}", bind_addr_port)?;
    writeln!(file, "cache-size: 10")?;
    writeln!(file, "log-level: 10")?;
    writeln!(file, "zcash-conf-path: ../conf/zcash.conf")?;

    Ok(())
}

fn write_zcash_conf(dir: &std::path::Path, rpcport: u16) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = dir.join("zcash.conf");
    let mut file = std::fs::File::create(file_path)?;
    writeln!(file, "regtest=1")?;
    writeln!(file, "nuparams=5ba81b19:1 # Overwinter")?;
    writeln!(file, "nuparams=76b809bb:1 # Sapling")?;
    writeln!(file, "nuparams=2bb40e60:1 # Blossom")?;
    writeln!(file, "nuparams=f5b9230b:1 # Heartwood")?;
    writeln!(file, "nuparams=e9ff75a6:1 # Canopy")?;
    writeln!(file, "nuparams=c2d6d0b4:1 # NU5")?;
    writeln!(file, "txindex=1")?;
    writeln!(file, "insightexplorer=1")?;
    writeln!(file, "experimentalfeatures=1")?;
    writeln!(file, "rpcuser=xxxxxx")?;
    writeln!(file, "rpcpassword=xxxxxx")?;
    writeln!(file, "rpcport={}", rpcport)?;
    writeln!(file, "rpcallowip=127.0.0.1")?;
    writeln!(file, "listen=0")?;
    writeln!(file, "minetolocalwallet=0")?;
    // writeln!(file, "mineraddress=zregtestsapling1fmq2ufux3gm0v8qf7x585wj56le4wjfsqsj27zprjghntrerntggg507hxh2ydcdkn7sx8kya7p")?; // USE FOR SAPLING.
    writeln!(file, "mineraddress=uregtest1zkuzfv5m3yhv2j4fmvq5rjurkxenxyq8r7h4daun2zkznrjaa8ra8asgdm8wwgwjvlwwrxx7347r8w0ee6dqyw4rufw4wg9djwcr6frzkezmdw6dud3wsm99eany5r8wgsctlxquu009nzd6hsme2tcsk0v3sgjvxa70er7h27z5epr67p5q767s2z5gt88paru56mxpm6pwz0cu35m")?;

    Ok(())
}

fn create_temp_conf_files(
    lwd_port: u16,
    rpcport: u16,
) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let temp_dir = tempfile::Builder::new()
        .prefix("zingoproxytest")
        .tempdir()?;
    let conf_dir = temp_dir.path().join("conf");
    std::fs::create_dir(&conf_dir)?;
    write_lightwalletd_yml(&conf_dir, lwd_port)?;
    write_zcash_conf(&conf_dir, rpcport)?;
    Ok(temp_dir)
}

/// Returns the zcash address of the Zingolib::lightclient.
pub async fn get_zingo_address(
    zingo_client: &zingolib::lightclient::LightClient,
    pool: &str,
) -> String {
    zingolib::get_base_address!(zingo_client, pool)
}

/// Starts Zingolib::lightclients's mempool monitor.
pub async fn start_zingo_mempool_monitor(zingo_client: &zingolib::lightclient::LightClient) {
    let zingo_client_saved = zingo_client.export_save_buffer_async().await.unwrap();
    let zingo_client_loaded = std::sync::Arc::new(
        zingolib::lightclient::LightClient::read_wallet_from_buffer_async(
            zingo_client.config(),
            &zingo_client_saved[..],
        )
        .await
        .unwrap(),
    );
    zingolib::lightclient::LightClient::start_mempool_monitor(zingo_client_loaded.clone());
    // This seems to be long enough for the mempool monitor to kick in (from zingolib).
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
}

/// Zingo-Proxy wrapper for Zingolib's Pool Enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyPool {
    /// Orchard pool.
    Orchard,
    /// Sapling pool
    Sapling,
    /// Transparent poool.
    Transparent,
}

impl From<ProxyPool> for zingolib::wallet::Pool {
    fn from(test_pool: ProxyPool) -> Self {
        match test_pool {
            ProxyPool::Orchard => zingolib::wallet::Pool::Orchard,
            ProxyPool::Sapling => zingolib::wallet::Pool::Sapling,
            ProxyPool::Transparent => zingolib::wallet::Pool::Transparent,
        }
    }
}

impl From<zingolib::wallet::Pool> for ProxyPool {
    fn from(pool: zingolib::wallet::Pool) -> Self {
        match pool {
            zingolib::wallet::Pool::Orchard => ProxyPool::Orchard,
            zingolib::wallet::Pool::Sapling => ProxyPool::Sapling,
            zingolib::wallet::Pool::Transparent => ProxyPool::Transparent,
        }
    }
}
