//! Integration tests for zingo-Proxy.
//! Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

use std::sync::{atomic::AtomicBool, Arc};
use zingo_netutils::GrpcConnector;
use zingo_testutils::{drop_test_manager, get_proxy_uri, launch_test_manager};

mod proxy {
    use super::*;

    #[tokio::test]
    async fn connect_to_lwd_get_info() {
        let online = Arc::new(AtomicBool::new(true));

        let (_regtest_manager, regtest_handles, _handles, proxy_port, _nym_addr) =
            launch_test_manager(online.clone()).await;

        let proxy_uri = get_proxy_uri(proxy_port);
        println!("{}", proxy_uri);

        let lightd_info = GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .unwrap()
            .get_lightd_info(zcash_client_backend::proto::service::Empty {})
            .await
            .unwrap();
        println!("{:#?}", lightd_info.into_inner());

        drop_test_manager(regtest_handles, online).await
    }
}

#[cfg(feature = "nym")]
mod nym {
    // TODO: Add Nym Tests.
}

#[cfg(feature = "darkside")]
mod darkside {
    // TODO: Add darkside tests.
}
