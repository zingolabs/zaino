//! Zingo-Proxy server implementation.

use crate::{nym_server::nym_serve, server::spawn_server};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use zingo_rpc::nym::utils::nym_spawn;

/// Launches test Zingo_Proxy server.
pub async fn spawn_proxy(
    proxy_port: &'static u16,
    lwd_port: &'static u16,
    zebrad_port: &'static u16,
) -> (Vec<JoinHandle<()>>, Arc<Notify>, Option<String>) {
    let notify = Arc::new(Notify::new());
    let mut handles = vec![];
    let nym_addr = Arc::new(Mutex::new(None::<String>));
    let nym_addr_out: Option<String>;

    #[cfg(feature = "nym")]
    {
        let nym_notify_clone = notify.clone();
        let nym_addr_clone = nym_addr.clone();

        let nym_proxy_handle = tokio::spawn(async move {
            let path = "/tmp/nym_server";
            let mut server = nym_spawn(path).await;
            let address = server.nym_address().to_string();

            let mut addr_lock = nym_addr_clone.lock().await;
            *addr_lock = Some(address.clone());

            nym_serve(&mut server).await;
        });
        tokio::select! {
            _ = nym_notify_clone.notified() => {
                println!("Zingo-Proxy(Nym) is shutting down.");
                nym_proxy_handle.abort();
            }
        }

        handles.push(nym_proxy_handle);
    }

    let notify_clone = notify.clone();

    let proxy_handle = tokio::spawn(async move {
        spawn_server(proxy_port, lwd_port, zebrad_port).await;
    });
    tokio::select! {
        _ = notify_clone.notified() => {
            println!("Zingo-Proxy is shutting down.");
            proxy_handle.abort();
        }
    }
    handles.push(proxy_handle);

    #[cfg(feature = "nym")]
    {
        let nym_addr_proxy_clone = nym_addr.clone();
        nym_addr_out = {
            let addr_lock = nym_addr_proxy_clone.lock().await;
            addr_lock.clone()
        };
    }
    #[cfg(not(feature = "nym"))]
    {
        nym_addr_out = None;
    }

    (handles, notify, nym_addr_out)
}

/// Closes test Zingo-Proxy servers currently active.
pub async fn close_proxy(handles: Vec<JoinHandle<()>>, notify: Arc<Notify>) {
    notify.notify_waiters();
    for handle in handles {
        let _ = handle.await;
    }
}
