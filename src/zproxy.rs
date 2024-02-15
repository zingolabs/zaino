// zproxy.rs [lib]
// use: zproxy lib
//

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{atomic::AtomicBool, Arc},
};

use http::Uri;
use zcash_client_backend::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{
        compact_tx_streamer_server::{CompactTxStreamer, CompactTxStreamerServer},
        Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Empty, Exclude,
        GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList, GetSubtreeRootsArg,
        LightdInfo, PingResponse, RawTransaction, SendResponse, SubtreeRoot,
        TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

macro_rules! define_grpc_passthrough {
    (fn
        $name:ident(
            &$self:ident$(,$($arg:ident: $argty:ty,)*)?
        ) -> $ret:ty
    ) => {
        #[must_use]
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn $name<'life0, 'async_trait>(&'life0 $self$($(, $arg: $argty)*)?) ->
           ::core::pin::Pin<Box<
                dyn ::core::future::Future<
                    Output = ::core::result::Result<
                        ::tonic::Response<$ret>,
                        ::tonic::Status
                >
            > + ::core::marker::Send + 'async_trait
        >>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            println!("received call of {}", stringify!($name));
            Box::pin(async {
                ::zingo_netutils::GrpcConnector::new($self.lightwalletd_uri.clone())
                    .get_client()
                    .await
                    .expect("Proxy server failed to create client")
                    .$name($($($arg),*)?)
                    .await
            })
        }
    };
}

pub struct ProxyServer {
    pub lightwalletd_uri: http::Uri,
    pub zebrad_uri: http::Uri,
    pub online: Arc<AtomicBool>,
}

impl ProxyServer {
    pub fn serve(
        self,
        port: impl Into<u16> + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        println!("Starting server task");
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self);
            let sockaddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port.into());
            println!("Proxy listening on {sockaddr}");
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve(sockaddr)
                .await
        })
    }

    pub fn new(lightwalletd_uri: http::Uri, zebrad_uri: http::Uri) -> Self {
        Self {
            lightwalletd_uri,
            zebrad_uri,
            online: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl CompactTxStreamer for ProxyServer {
    // fn get_latest_block<'life0, 'async_trait>(
    //     &'life0 self,
    //     request: tonic::Request<zcash_client_backend::proto::service::ChainSpec>,
    // ) -> core::pin::Pin<
    //     Box<
    //         dyn core::future::Future<
    //                 Output = std::result::Result<
    //                     tonic::Response<zcash_client_backend::proto::service::BlockId>,
    //                     tonic::Status,
    //                 >,
    //             > + core::marker::Send
    //             + 'async_trait,
    //     >,
    // >
    // where
    //     'life0: 'async_trait,
    //     Self: 'async_trait,
    // {
    //     println!("received call of get_latest_block");
    //     Box::pin(async {
    //         let zebrad_info = ::zingo_netutils::GrpcConnector::new(self.zebrad_uri.clone())
    //             .get_client()
    //             .await;
    //         todo!("get_latest_block not yet implemented")
    //     })
    // }

    define_grpc_passthrough!(
        fn get_latest_block(
            &self,
            request: tonic::Request<ChainSpec>,
        ) -> BlockId
    );

    define_grpc_passthrough!(
        fn get_block(
            &self,
            request: tonic::Request<BlockId>,
        ) -> CompactBlock
    );

    #[doc = "Server streaming response type for the GetBlockRange method."]
    type GetBlockRangeStream = tonic::Streaming<CompactBlock>;

    define_grpc_passthrough!(
        fn get_block_range(
            &self,
            request: tonic::Request<BlockRange>,
        ) -> Self::GetBlockRangeStream
    );

    define_grpc_passthrough!(
        fn get_transaction(
            &self,
            request: tonic::Request<TxFilter>,
        ) -> RawTransaction
    );

    define_grpc_passthrough!(
        fn send_transaction(
            &self,
            request: tonic::Request<RawTransaction>,
        ) -> SendResponse
    );

    #[doc = "Server streaming response type for the GetTaddressTxids method."]
    type GetTaddressTxidsStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_taddress_txids(
            &self,
            request: tonic::Request<TransparentAddressBlockFilter>,
        ) -> Self::GetTaddressTxidsStream
    );

    define_grpc_passthrough!(
        fn get_taddress_balance(
            &self,
            request: tonic::Request<AddressList>,
        ) -> Balance
    );

    /// This isn't easily definable with the macro, and I beleive it to be unused
    #[must_use]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn get_taddress_balance_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<tonic::Streaming<Address>>,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<tonic::Response<Balance>, tonic::Status>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!("this isn't expected to be called. Please implement this if you need it")
    }

    #[doc = "Server streaming response type for the GetMempoolTx method."]
    type GetMempoolTxStream = tonic::Streaming<CompactTx>;

    define_grpc_passthrough!(
        fn get_mempool_tx(
            &self,
            request: tonic::Request<Exclude>,
        ) -> Self::GetMempoolTxStream
    );

    #[doc = "Server streaming response type for the GetMempoolStream method."]
    type GetMempoolStreamStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_mempool_stream(
            &self,
            request: tonic::Request<Empty>,
        ) -> Self::GetMempoolStreamStream
    );

    define_grpc_passthrough!(
        fn get_tree_state(
            &self,
            request: tonic::Request<BlockId>,
        ) -> TreeState
    );

    define_grpc_passthrough!(
        fn get_address_utxos(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> GetAddressUtxosReplyList
    );

    #[doc = "Server streaming response type for the GetAddressUtxosStream method."]
    type GetAddressUtxosStreamStream = tonic::Streaming<GetAddressUtxosReply>;

    define_grpc_passthrough!(
        fn get_address_utxos_stream(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> tonic::Streaming<GetAddressUtxosReply>
    );

    define_grpc_passthrough!(
        fn get_lightd_info(
            &self,
            request: tonic::Request<Empty>,
        ) -> LightdInfo
    );

    define_grpc_passthrough!(
        fn ping(
            &self,
            request: tonic::Request<zcash_client_backend::proto::service::Duration>,
        ) -> PingResponse
    );

    define_grpc_passthrough!(
        fn get_block_nullifiers(
            &self,
            request: tonic::Request<BlockId>,
        ) -> CompactBlock
    );

    define_grpc_passthrough!(
        fn get_block_range_nullifiers(
            &self,
            request: tonic::Request<BlockRange>,
        ) -> Self::GetBlockRangeNullifiersStream
    );
    #[doc = " Server streaming response type for the GetBlockRangeNullifiers method."]
    type GetBlockRangeNullifiersStream = tonic::Streaming<CompactBlock>;

    define_grpc_passthrough!(
        fn get_latest_tree_state(
            &self,
            request: tonic::Request<Empty>,
        ) -> TreeState
    );

    define_grpc_passthrough!(
        fn get_subtree_roots(
            &self,
            request: tonic::Request<GetSubtreeRootsArg>,
        ) -> Self::GetSubtreeRootsStream
    );

    #[doc = " Server streaming response type for the GetSubtreeRoots method."]
    type GetSubtreeRootsStream = tonic::Streaming<SubtreeRoot>;
}

pub async fn spawn_server(
    proxy_port: u16,
    lwd_port: u16,
    zebrad_port: u16,
) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let lwd_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{lwd_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let zebra_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{zebrad_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let server = ProxyServer::new(lwd_uri, zebra_uri);
    server.serve(proxy_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    /// Note: This test currently requires a manual boot of zcashd + lightwalletd to run
    async fn connect_to_lwd_get_info() {
        let server_port = 8080;
        let _server_handle = spawn_server(server_port, 9067, 18232).await;
        sleep(Duration::from_secs(3)).await;
        let proxy_uri = Uri::builder()
            .scheme("http")
            .authority(format!("localhost:{server_port}"))
            .path_and_query("")
            .build()
            .unwrap();
        println!("{}", proxy_uri);
        let lightd_info = zingo_netutils::GrpcConnector::new(proxy_uri)
            .get_client()
            .await
            .unwrap()
            .get_lightd_info(Empty {})
            .await
            .unwrap();
        println!("{:#?}", lightd_info.into_inner());
    }
}
