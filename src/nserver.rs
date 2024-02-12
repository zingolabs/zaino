// nserver.rs [lib]
// use: nserver lib
//

use std::sync::Arc;

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

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::{Request, Response};
use zingo_netutils::GrpcConnector;

pub async fn tcp_listener(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    println!("Server listening on {}", addr);

    loop {
        let (socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket).await {
                eprintln!("Failed to handle connection: {}", e);
            }
        });
    }
}

pub async fn handle_connection(mut socket: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = vec![0u8; 16384]; // Adjust buffer size as needed
    let bytes_read = socket.read(&mut buf).await?;
    if bytes_read == 0 {
        return Err("Connection closed by client".into());
    }

    //print request for testing
    println!("request received: {:?}", &buf[..bytes_read]);
    println!("request length: {}", &buf[..bytes_read].len());

    let request = RawTransaction::decode(&buf[..bytes_read])?;
    let response = process_request(&request).await?;
    let mut response_buf = Vec::new();
    response.encode(&mut response_buf)?;

    socket.write_all(&response_buf).await?;
    socket.flush().await?;
    Ok(())
}

pub async fn process_request(
    request: &RawTransaction,
) -> Result<SendResponse, Box<dyn std::error::Error>> {
    let zproxy_port = 8080;
    let zproxy_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{zproxy_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let client = Arc::new(GrpcConnector::new(zproxy_uri));

    let mut cmp_client = client
        .get_client()
        .await
        .map_err(|e| format!("Error getting client: {:?}", e))?;
    let grpc_request = Request::new(request.clone());

    let response = cmp_client
        .send_transaction(grpc_request)
        .await
        .map_err(|e| format!("Send Error: {}", e))?;
    Ok(response.into_inner())
}
