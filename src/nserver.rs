// nserver.rs [lib]
// use: nserver lib
//

use std::sync::Arc;

use http::Uri;
use nym_sdk::mixnet::{
    MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient, ReconstructedMessage,
    StoragePaths,
};
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::{Request, Response};
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

    //print response for testing
    println!("response sent: {:?}", &buf[..bytes_read]);
    println!("response length: {}", &buf[..bytes_read].len());

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

pub async fn nym_serve(client: &mut MixnetClient) {
    let mut request_in: Vec<ReconstructedMessage> = Vec::new();
    loop {
        while let Some(request_nym) = client.wait_for_messages().await {
            if request_nym.is_empty() {
                continue;
            }
            request_in = request_nym;
            break;
        }
        let request_vu8 = request_in
            .first()
            .map(|r| r.message.clone())
            .ok_or_else(|| "No response received from the nym network".to_string())
            .unwrap();

        //print request for testing
        println!("request received: {:?}", &request_vu8[..]);
        println!("request length: {}", &request_vu8[..].len());

        let request = RawTransaction::decode(&request_vu8[..]).unwrap();
        let response = process_request(&request).await.unwrap();
        let mut response_vu8 = Vec::new();
        response.encode(&mut response_vu8).unwrap();

        //print response for testing
        println!("response sent: {:?}", &response_vu8[..]);
        println!("response length: {}", &response_vu8[..].len());

        let return_recipient = AnonymousSenderTag::try_from_base58_string(
            request_in[0].sender_tag.unwrap().to_base58_string(),
        )
        .unwrap();
        client
            .send_reply(return_recipient, response_vu8)
            .await
            .unwrap();
    }
}
