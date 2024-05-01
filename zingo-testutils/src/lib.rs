//! Utility functions for Zingo-Proxy Testing.

// use nym_sphinx_addressing::clients::Recipient;
use nym_sdk::mixnet::{MixnetClient, Recipient};

use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use zingo_rpc::nym::utils::{nym_close, nym_spawn};
use zingoproxylib::{nym_server::nym_serve, server::spawn_server};

/// Launches test Zingo_Proxy server.
async fn spawn_proxys(
    proxy_port: &'static u16,
    lwd_port: &'static u16,
    zebrad_port: &'static u16,
    nym_addr: Option<&'static Recipient>,
) -> (Vec<JoinHandle<()>>, Arc<Notify>) {
    let notify = Arc::new(Notify::new());
    let mut handles = vec![];
    let nym_addr = Arc::new(Mutex::new(None));

    #[cfg(feature = "nym_test")]
    {
        let nym_proxy_notify_clone = notify.clone();
        let nym_addr_clone = nym_addr.clone();

        let nym_proxy_handle = tokio::spawn(async move {
            let path = "/tmp/nym_server";
            let mut server = nym_spawn(path).await;
            let address = server.nym_address();

            let mut addr_lock = nym_addr_clone.lock().await;
            *addr_lock = Some(address.clone());

            nym_serve(&mut server).await;
        });
        tokio::select! {
            _ = nym_proxy_notify_clone.notified() => {
                println!("Zingo-Proxy is shutting down.");
                nym_proxy_handle.abort();
            }
        }

        handles.push(nym_proxy_handle);
    }

    let notify_clone = notify.clone();

    let proxy_handle = tokio::spawn(async move {
        #[cfg(feature = "nym_test")]
        {
            let nym_addr_proxy_clone = nym_addr.clone();
            let nym_addr = {
                let addr_lock = nym_addr_proxy_clone.lock().await;
                addr_lock.clone()
            };
        }
        // TODO: Expose nym server address.
        spawn_server(proxy_port, lwd_port, zebrad_port).await;
    });
    tokio::select! {
        _ = notify_clone.notified() => {
            println!("Zingo-Proxy is shutting down.");
            proxy_handle.abort();
        }
    }
    handles.push(proxy_handle);

    (handles, notify)
}

/// Closes test Zingo-Proxy servers currently active.
async fn close_proxys(handles: Vec<JoinHandle<()>>, notify: Arc<Notify>) {
    notify.notify_waiters();
    for handle in handles {
        let _ = handle.await;
    }
}
