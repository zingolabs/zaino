//! End to end tests for zingo-Proxy.
//! Uses ZingoLib as its test wallet. Currently uses ZCashD as ZebraD has not yet implemented Regtest Mode.

#![forbid(unsafe_code)]

async fn launch_proxy(proxy_port: u16, lwd_port: u16, zebrad_port: u16) {
    //launch
}

mod proxy_tests {
    #[tokio::test]
    async fn connect_to_lwd_get_info() {
        // launch proxy/nym server

        // load wallet (scenario)

        // get info from server

        // assert info
    }
}

mod nym_tests {
    //todo!
}

