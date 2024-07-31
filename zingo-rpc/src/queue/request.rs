//! Request types.

use std::time::SystemTime;

use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use tokio::net::TcpStream;

use crate::{nym::utils::read_nym_request_data, queue::error::RequestError};

/// Requests queuing metadata.
#[derive(Debug, Clone)]
struct QueueData {
    // / Exclusive request id.
    // request_id: u64, // TODO: implement with request queue (implement exlusive request_id generator in queue object).
    /// Time which the request was received.
    time_received: SystemTime,
    /// Number of times the request has been requeued.
    requeue_attempts: u32,
}

impl QueueData {
    /// Returns a new instance of QueueData.
    fn new() -> Self {
        QueueData {
            time_received: SystemTime::now(),
            requeue_attempts: 0,
        }
    }

    /// Increases the requeue attempts for the request.
    pub fn increase_requeues(&mut self) {
        self.requeue_attempts += 1;
    }

    /// Returns the duration sunce the request was received.
    fn duration(&self) -> Result<std::time::Duration, RequestError> {
        self.time_received.elapsed().map_err(RequestError::from)
    }

    /// Returns the number of times the request has been requeued.
    fn requeues(&self) -> u32 {
        self.requeue_attempts
    }
}

/// Nym request data.
#[derive(Debug, Clone)]
pub struct NymRequest {
    id: u64,
    method: String,
    metadata: AnonymousSenderTag,
    body: Vec<u8>,
}

impl NymRequest {
    /// Returns the client assigned id for this request, only used to construct response.
    pub fn client_id(&self) -> u64 {
        self.id
    }

    /// Returns the RPC being called by the request.
    pub fn method(&self) -> String {
        self.method.clone()
    }

    /// Returns request metadata including sender data.
    pub fn metadata(&self) -> AnonymousSenderTag {
        self.metadata
    }

    /// Returns the request body.
    pub fn body(&self) -> Vec<u8> {
        self.body.clone()
    }
}

/// TcpStream holing an incoming gRPC request.
#[derive(Debug)]
pub struct TcpRequest(TcpStream);

impl TcpRequest {
    /// Returns the underlying TcpStream help by the request
    pub fn get_stream(self) -> TcpStream {
        self.0
    }
}

/// Requests originating from the Nym server.
#[derive(Debug, Clone)]
pub struct NymServerRequest {
    queuedata: QueueData,
    request: NymRequest,
}

impl NymServerRequest {
    /// Returns the underlying request.
    pub fn get_request(&self) -> NymRequest {
        self.request.clone()
    }
}

/// Requests originating from the Tcp server.
#[derive(Debug)]
pub struct TcpServerRequest {
    queuedata: QueueData,
    request: TcpRequest,
}

impl TcpServerRequest {
    /// Returns the underlying request.
    pub fn get_request(self) -> TcpRequest {
        self.request
    }
}

/// Zingo-Proxy request, used by request queue.
#[derive(Debug)]
pub enum ZingoProxyRequest {
    /// Requests originating from the Nym server.
    NymServerRequest(NymServerRequest),
    /// Requests originating from the gRPC server.
    TcpServerRequest(TcpServerRequest),
}

impl ZingoProxyRequest {
    /// Creates a ZingoProxyRequest from an encoded gRPC service call, recieved by the Nym server.
    pub fn new_from_nym(metadata: AnonymousSenderTag, bytes: &[u8]) -> Result<Self, RequestError> {
        let (id, method, body) = read_nym_request_data(bytes)?;
        Ok(ZingoProxyRequest::NymServerRequest(NymServerRequest {
            queuedata: QueueData::new(),
            request: NymRequest {
                id,
                method,
                metadata,
                body: body.to_vec(),
            },
        }))
    }

    /// Creates a ZingoProxyRequest from a gRPC service call, recieved by the gRPC server.
    ///
    /// TODO: implement proper functionality along with queue.
    pub fn new_from_grpc(stream: TcpStream) -> Self {
        ZingoProxyRequest::TcpServerRequest(TcpServerRequest {
            queuedata: QueueData::new(),
            request: TcpRequest(stream),
        })
    }

    /// Increases the requeue attempts for the request.
    pub fn increase_requeues(&mut self) {
        match self {
            ZingoProxyRequest::NymServerRequest(ref mut req) => req.queuedata.increase_requeues(),
            ZingoProxyRequest::TcpServerRequest(ref mut req) => req.queuedata.increase_requeues(),
        }
    }

    /// Returns the duration sunce the request was received.
    pub fn duration(&self) -> Result<std::time::Duration, RequestError> {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.queuedata.duration(),
            ZingoProxyRequest::TcpServerRequest(ref req) => req.queuedata.duration(),
        }
    }

    /// Returns the number of times the request has been requeued.
    pub fn requeues(&self) -> u32 {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.queuedata.requeues(),
            ZingoProxyRequest::TcpServerRequest(ref req) => req.queuedata.requeues(),
        }
    }
}
