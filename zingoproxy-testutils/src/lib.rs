//! Utility functions for Zingo-Proxy Testing.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use std::io::Write;

/// Configuration data for Zingo-Proxy Tests.
pub struct TestManager {
    /// Temporary Directory for nym, zcashd and lightwalletd configuration and regtest data.
    pub temp_conf_dir: tempfile::TempDir,
    /// Zingolib regtest manager.
    pub regtest_manager: zingo_testutils::regtest::RegtestManager,
    /// Zing-Proxy gRPC listen port.
    pub proxy_port: u16,
    /// Zingo-Proxy Nym listen address.
    pub nym_addr: Option<String>,
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
        let zcashd_port = portpicker::pick_unused_port().expect("No ports free");
        let proxy_port = portpicker::pick_unused_port().expect("No ports free");

        let temp_conf_dir = create_temp_conf_files(lwd_port, zcashd_port).unwrap();
        let temp_conf_path = temp_conf_dir.path().to_path_buf();
        let nym_conf_path = temp_conf_path.join("nym");

        set_custom_drops(online.clone(), Some(temp_conf_path.clone()));

        let regtest_manager = zingo_testutils::regtest::RegtestManager::new(temp_conf_path.clone());
        let regtest_handler = regtest_manager
            .launch(true)
            .expect("Failed to start regtest services");

        let (proxy_handler, nym_addr) = zingoproxylib::proxy::spawn_proxy(
            &proxy_port,
            &lwd_port,
            &zcashd_port,
            nym_conf_path.to_str().unwrap(),
            online.clone(),
        )
        .await;

        (
            TestManager {
                temp_conf_dir,
                regtest_manager,
                proxy_port,
                nym_addr,
                online,
            },
            regtest_handler,
            proxy_handler,
        )
    }
}

/// Closes test manager child processes, optionally cleans configuration and log files for test.
pub async fn drop_test_manager(
    child_process_handler: zingo_testutils::regtest::ChildProcessHandler,
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    online.store(false, std::sync::atomic::Ordering::SeqCst);
    drop(child_process_handler);
}

fn set_custom_drops(
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
    temp_conf_path: Option<std::path::PathBuf>,
) {
    let online_panic = online.clone();
    let online_ctrlc = online.clone();
    let temp_conf_path_panic = temp_conf_path.clone();
    let temp_conf_path_ctrlc = temp_conf_path.clone();

    std::panic::set_hook(Box::new(move |panic_info| {
        if let Some(location) = panic_info.location() {
            println!(
                "Panic occurred in file '{}' at line {}",
                location.file(),
                location.line()
            );
        } else {
            println!("Panic occurred but no location information available.");
        };
        online_panic.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(ref path) = temp_conf_path_panic {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!("Failed to delete temporary directory: {:?}", e);
            }
        }
    }));
    ctrlc::set_handler(move || {
        println!("Received Ctrl+C, exiting.");
        online_ctrlc.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(ref path) = temp_conf_path_ctrlc {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!("Failed to delete temporary directory: {:?}", e);
            }
        }
        std::process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");
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
    writeln!(file, "mineraddress=zregtestsapling1fp58yvw40ytns3qrcc4p58ga9xunqglf5al6tl49fdlq3yrc2wk99dwrnxmhcyw5nlsqqa680rq")?;
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

/// Returns zingo-proxy listen porn.
pub fn get_proxy_uri(proxy_port: u16) -> http::Uri {
    http::Uri::builder()
        .scheme("http")
        .authority(format!("127.0.0.1:{proxy_port}"))
        .path_and_query("")
        .build()
        .unwrap()
}
