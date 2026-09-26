use futures::FutureExt as _;
use jsonrpsee::{
    server::middleware::rpc::RpcServiceT,
    types::{ErrorCode, ErrorObject, Id},
    MethodResponse,
};
use zaino_state::jsonrpc_types::LegacyCode;

/// A JSON-RPC middleware that reports jsonrpsee's invalid-params rejections with zcashd's code, which some clients match on.
#[derive(Clone)]
pub(crate) struct FixRpcResponseMiddleware<S> {
    service: S,
}

impl<S> FixRpcResponseMiddleware<S> {
    /// Wraps `service`.
    pub(crate) fn new(service: S) -> Self {
        Self { service }
    }
}

impl<'a, S> RpcServiceT<'a> for FixRpcResponseMiddleware<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = futures::future::Map<S::Future, fn(MethodResponse) -> MethodResponse>;

    fn call(&self, request: jsonrpsee::types::Request<'a>) -> Self::Future {
        self.service
            .call(request)
            .map(with_zcashd_error_code as fn(MethodResponse) -> MethodResponse)
    }
}

/// Replaces an invalid-params error code with zcashd's `Misc` code, keeping the id and the message.
fn with_zcashd_error_code(response: MethodResponse) -> MethodResponse {
    if response.as_error_code() != Some(ErrorCode::InvalidParams.code()) {
        return response;
    }

    let new_error_code = i32::from(LegacyCode::Misc);
    tracing::debug!(
        "Replacing RPC error: {} with {new_error_code}",
        ErrorCode::InvalidParams.code()
    );
    let json: serde_json::Value = serde_json::from_str(response.into_parts().0.as_str())
        .expect("a jsonrpsee response is valid json");
    let id = match &json["id"] {
        serde_json::Value::Null => Some(Id::Null),
        serde_json::Value::Number(n) => n.as_u64().map(Id::Number),
        serde_json::Value::String(s) => Some(Id::Str(s.into())),
        _ => None,
    }
    .expect("a jsonrpsee error response carries an id");

    MethodResponse::error(
        id,
        ErrorObject::borrowed(
            new_error_code,
            json.get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .unwrap_or("Invalid params"),
            None,
        ),
    )
}

#[cfg(test)]
mod with_zcashd_error_code {
    use super::*;

    /// The response a client receives, as JSON.
    fn json(response: MethodResponse) -> serde_json::Value {
        serde_json::from_str(response.into_parts().0.as_str()).expect("a response is valid json")
    }

    #[test]
    fn reports_invalid_params_as_misc_keeping_the_id_and_message() {
        let response = MethodResponse::error(
            Id::Number(7),
            ErrorObject::owned(ErrorCode::InvalidParams.code(), "bad height", None::<()>),
        );

        let fixed = json(super::with_zcashd_error_code(response));

        assert_eq!(fixed["id"], 7);
        assert_eq!(fixed["error"]["code"], i32::from(LegacyCode::Misc));
        assert_eq!(fixed["error"]["message"], "bad height");
    }

    #[test]
    fn leaves_every_other_error_and_every_result_unchanged() {
        let other = MethodResponse::error(
            Id::Str("lwd".into()),
            ErrorObject::owned(i32::from(LegacyCode::InvalidParameter), "nope", None::<()>),
        );
        let result = MethodResponse::response(
            Id::Null,
            jsonrpsee::ResponsePayload::success(1u32),
            usize::MAX,
        );

        assert_eq!(
            json(super::with_zcashd_error_code(other))["error"]["code"],
            i32::from(LegacyCode::InvalidParameter)
        );
        assert_eq!(json(super::with_zcashd_error_code(result))["result"], 1);
    }
}
