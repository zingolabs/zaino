// nproxy.rs [lib]
// use: nproxy lib
//

use crate::nym_utils::{deserialize_response, forward_over_tcp, serialize_request};
use http::Uri;
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{atomic::AtomicBool, Arc},
};
use tonic::{async_trait, Request, Response, Status};
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

#[async_trait]
impl CompactTxStreamer for ProxyServer {
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

    async fn send_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        println!("Received call to send_transaction");

        // Serialize the RawTransaction request
        let serialized_request = match serialize_request(&request.into_inner()).await {
            Ok(data) => data,
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to serialize request: {}",
                    e
                )))
            }
        };

        //print request for testing:
        println!("Requests being sent: {:?}", serialized_request);
        println!("request length: {}", serialized_request.len());

        // Forward the serialized data over TCP
        let addr = "127.0.0.1:9090";
        let response_data = match forward_over_tcp(addr, &serialized_request).await {
            Ok(data) => data,
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to forward transaction over TCP: {}",
                    e
                )))
            }
        };

        // deserialize SendResponse
        let response: SendResponse = match deserialize_response(response_data.as_slice()).await {
            Ok(res) => res,
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to decode response: {}",
                    e
                )))
            }
        };

        Ok(Response::new(response))
    }

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
