// nym_utils.rs [lib]
// use: nym-proxy utils
//

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

pub async fn deserialize_response<T: prost::Message + Default>(
    data: &[u8],
) -> Result<T, Box<dyn std::error::Error>> {
    T::decode(data).map_err(Into::into)
}
