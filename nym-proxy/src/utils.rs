// nym_utils.rs [lib]
// use: nym-proxy utils
//

use nym_sdk::mixnet::{
    MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient, ReconstructedMessage,
    StoragePaths,
};
use std::path::PathBuf;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub async fn serialize_request<T: prost::Message>(
    request: &T,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    request.encode(&mut buf)?;
    Ok(buf)
}

pub async fn deserialize_response<T: prost::Message + Default>(
    data: &[u8],
) -> Result<T, Box<dyn std::error::Error>> {
    T::decode(data).map_err(Into::into)
}

pub async fn forward_over_tcp(
    addr: &str,
    data: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(response)
}

pub async fn nym_spawn(str_path: &str) -> MixnetClient {
    //nym_bin_common::logging::setup_logging();
    MixnetClientBuilder::new_with_default_storage(
        StoragePaths::new_from_dir(PathBuf::from(str_path)).unwrap(),
    )
    .await
    .unwrap()
    .build()
    .unwrap()
    .connect_to_mixnet()
    .await
    .unwrap()
}

pub async fn nym_close(client: MixnetClient) {
    client.disconnect().await;
}

pub async fn nym_forward(
    client: &mut MixnetClient,
    recipient_address: &str,
    message: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let recipient: Recipient =
        Recipient::try_from_base58_string(recipient_address.to_string()).unwrap();
    client.send_plain_message(recipient, message).await.unwrap();

    let mut nym_response: Vec<ReconstructedMessage> = Vec::new();
    while let Some(response_in) = client.wait_for_messages().await {
        if response_in.is_empty() {
            continue;
        }
        nym_response = response_in;
        break;
    }
    let response_out = nym_response
        .first()
        .map(|r| r.message.clone())
        .ok_or_else(|| "No response received from the nym network".to_string())?;
    Ok(response_out)
}
