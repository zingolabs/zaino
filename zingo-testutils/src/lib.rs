//! Utility functions for Zingo-Proxy Testing.

use std::io::Write;

fn write_lightwalletd_yml(
    dir: &std::path::Path,
    bind_addr_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = dir.join("lightwalletd.yml");
    let mut file = std::fs::File::create(file_path)?;
    writeln!(file, "bind-addr: 127.0.0.1:{}", bind_addr_port)?;
    writeln!(file, "cache-size: 10")?;
    writeln!(file, "log-level: 10")?;
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
    writeln!(file, "minetolocalwallet=0")?;
    writeln!(file, "mineraddress=zregtestsapling1fp58yvw40ytns3qrcc4p58ga9xunqglf5al6tl49fdlq3yrc2wk99dwrnxmhcyw5nlsqqa680rq")?;
    Ok(())
}

fn create_temp_conf_files(
    lwd_port: u16,
    rpcport: u16,
) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let temp_dir = tempfile::TempDir::new()?;
    let conf_dir = temp_dir.path().join("conf");
    std::fs::create_dir(&conf_dir)?;
    write_lightwalletd_yml(&conf_dir, lwd_port)?;
    write_zcash_conf(&conf_dir, rpcport)?;
    Ok(temp_dir)
}

pub fn get_proxy_uri(proxy_port: u16) -> http::Uri {
    http::Uri::builder()
        .scheme("http")
        .authority(format!("127.0.0.1:{proxy_port}"))
        .path_and_query("")
        .build()
        .unwrap()
}

pub async fn launch_test_manager(
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (
    zingo_testutils::regtest::RegtestManager,
    zingo_testutils::regtest::ChildProcessHandler,
    Vec<tokio::task::JoinHandle<Result<(), tonic::transport::Error>>>,
    u16,
    Option<String>,
) {
    let lwd_port = portpicker::pick_unused_port().expect("No ports free");
    let zcashd_port = portpicker::pick_unused_port().expect("No ports free");
    let proxy_port = portpicker::pick_unused_port().expect("No ports free");

    let temp_conf_dir = create_temp_conf_files(lwd_port, zcashd_port).unwrap();

    let regtest_manager = zingo_testutils::regtest::RegtestManager::new(std::path::PathBuf::from(
        temp_conf_dir.path(),
    ));
    let regtest_handler = regtest_manager
        .launch(true)
        .expect("Failed to start regtest services");

    let (handles, nym_addr) =
        zingoproxylib::proxy::spawn_proxy(&proxy_port, &lwd_port, &zcashd_port, online).await;

    (
        regtest_manager,
        regtest_handler,
        handles,
        proxy_port,
        nym_addr,
    )
}

pub async fn drop_test_manager(
    child_process_handler: zingo_testutils::regtest::ChildProcessHandler,
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    zingoproxylib::proxy::close_proxy(online).await;
    drop(child_process_handler);
}
