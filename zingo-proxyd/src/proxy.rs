//! Zingo-Proxy server implementation.

use crate::{nym_server::NymServer, server::spawn_server};
use zingo_rpc::primitives::NymClient;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

use tonic::transport::Error as TonicError;

/// Launches test Zingo_Proxy server.
pub async fn spawn_proxy(
    proxy_port: &u16,
    lwd_port: &u16,
    zebrad_port: &u16,
    nym_conf_path: &str,
    online: Arc<AtomicBool>,
) -> (Vec<JoinHandle<Result<(), TonicError>>>, Option<String>) {
    let mut handles = vec![];
    let nym_addr_out: Option<String>;

    println!("Loading Zing-Proxy..");

    #[cfg(not(feature = "nym_poc"))]
    {
        let nym_server: NymServer = NymServer(NymClient::nym_spawn(nym_conf_path).await);
        nym_addr_out = Some(nym_server.0 .0.nym_address().to_string());

        let nym_proxy_handle = nym_server.serve(online.clone()).await;
        handles.push(nym_proxy_handle);
    }
    let proxy_handle = spawn_server(proxy_port, lwd_port, zebrad_port, online).await;
    handles.push(proxy_handle);

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    #[cfg(feature = "nym_poc")]
    {
        nym_addr_out = None;
    }
    (handles, nym_addr_out)
}

/// Closes test Zingo-Proxy servers currently active.
pub async fn close_proxy(online: Arc<AtomicBool>) {
    online.store(false, Ordering::SeqCst);
}
