//! Request types.

use std::time::SystemTime;

use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use tonic::metadata::MetadataMap;

use crate::nym::utils::read_nym_request_data;

/// Zingo-Proxy request errors.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Errors originating from incorrect enum types being called.
    #[error("Incorrect variant")]
    IncorrectVariant,
    /// System time errors.
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
}

/// Requests queuing metadata.
#[derive(Debug)]
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

/// Requests metadata either contains a return address for nym requests or a tonic MetaDataMap for gRPC requests.
#[derive(Debug, Clone)]
pub enum RequestMetaData {
    /// Return address for Nym requests.
    AnonSendrTag(AnonymousSenderTag),
    /// Metadata for gRPC requests.
    MetaDataMap(MetadataMap),
}

impl TryFrom<RequestMetaData> for AnonymousSenderTag {
    type Error = RequestError;

    fn try_from(value: RequestMetaData) -> Result<Self, Self::Error> {
        match value {
            RequestMetaData::AnonSendrTag(tag) => Ok(tag),
            _ => Err(RequestError::IncorrectVariant),
        }
    }
}

impl TryFrom<RequestMetaData> for MetadataMap {
    type Error = RequestError;

    fn try_from(value: RequestMetaData) -> Result<Self, Self::Error> {
        match value {
            RequestMetaData::MetaDataMap(map) => Ok(map),
            _ => Err(RequestError::IncorrectVariant),
        }
    }
}

/// Nym request data.
#[derive(Debug)]
struct NymRequest {
    id: u64,
    method: String,
    metadata: RequestMetaData,
    body: Vec<u8>,
}

/// Grpc request data.
/// TODO: Convert incoming gRPC calls to GrpcRequest before adding to queue (implement with request queue).
#[derive(Debug)]
struct GrpcRequest {
    id: u64,
    method: String,
    metadata: RequestMetaData,
    body: Vec<u8>,
}

/// Requests originating from the Nym server.
#[derive(Debug)]
pub struct NymServerRequest {
    queuedata: QueueData,
    request: NymRequest,
}

/// Requests originating from the gRPC server.
#[derive(Debug)]
pub struct GrpcServerRequest {
    queuedata: QueueData,
    request: GrpcRequest,
}

/// Zingo-Proxy request, used by request queue.
#[derive(Debug)]
pub enum ZingoProxyRequest {
    /// Requests originating from the Nym server.
    NymServerRequest(NymServerRequest),
    /// Requests originating from the gRPC server.
    GrpcServerRequest(GrpcServerRequest),
}

impl ZingoProxyRequest {
    /// Creates a ZingoProxyRequest from an encoded gRPC service call, recieved by the Nym server.
    pub fn new_from_nym(metadata: AnonymousSenderTag, bytes: &[u8]) -> Self {
        let (id, method, body) = read_nym_request_data(bytes).unwrap();
        ZingoProxyRequest::NymServerRequest(NymServerRequest {
            queuedata: QueueData::new(),
            request: NymRequest {
                id,
                method,
                metadata: RequestMetaData::AnonSendrTag(metadata),
                body: body.to_vec(),
            },
        })
    }

    /// Creates a ZingoProxyRequest from a gRPC service call, recieved by the gRPC server.
    ///
    /// TODO: implement proper functionality along with queue.
    pub fn new_from_grpc(metadata: MetadataMap, bytes: &[u8]) -> Self {
        ZingoProxyRequest::GrpcServerRequest(GrpcServerRequest {
            queuedata: QueueData::new(),
            request: GrpcRequest {
                id: 0,                      // TODO
                method: "TODO".to_string(), // TODO
                metadata: RequestMetaData::MetaDataMap(metadata),
                body: bytes.to_vec(),
            },
        })
    }

    /// Increases the requeue attempts for the request.
    pub fn increase_requeues(&mut self) {
        match self {
            ZingoProxyRequest::NymServerRequest(ref mut req) => req.queuedata.increase_requeues(),
            ZingoProxyRequest::GrpcServerRequest(ref mut req) => req.queuedata.increase_requeues(),
        }
    }

    /// Returns the duration sunce the request was received.
    pub fn duration(&self) -> Result<std::time::Duration, RequestError> {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.queuedata.duration(),
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.queuedata.duration(),
        }
    }

    /// Returns the number of times the request has been requeued.
    pub fn requeues(&self) -> u32 {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.queuedata.requeues(),
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.queuedata.requeues(),
        }
    }

    /// Returns the client assigned id for this request, only used to construct response.
    pub fn client_id(&self) -> u64 {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.request.id,
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.request.id,
        }
    }

    /// Returns the RPC being called by the request.
    pub fn method(&self) -> String {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.request.method.clone(),
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.request.method.clone(),
        }
    }

    /// Returns request metadata including sender data.
    pub fn metadata(&self) -> RequestMetaData {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.request.metadata.clone(),
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.request.metadata.clone(),
        }
    }

    /// Returns the number of times the request has been requeued.
    pub fn body(&self) -> Vec<u8> {
        match self {
            ZingoProxyRequest::NymServerRequest(ref req) => req.request.body.clone(),
            ZingoProxyRequest::GrpcServerRequest(ref req) => req.request.body.clone(),
        }
    }
}
