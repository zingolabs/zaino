//! Transaction methods: `SendTransaction` (submitted through the chain view), `GetTransaction`
//! (consensus bytes forwarded from a validator, usage.md "Why `GetTransaction` forwards")

use http::Response;
use tonic::{body::Body, Status};
use zaino_chainview::{ChainView, SubmitError};
use zaino_primitives::types::TransactionLocation;
use zaino_proto::proto::service::{self, RawTransaction};
use zaino_source::{ChainDataSource, GetTransactionError, QueryError};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::TxId;

use crate::wire::{self, decode_request, frame, unary_response};

/// `SendTransaction`: rejection = domain answer, only unreachable = a status (wallet tells "no"
/// from "unreachable")
pub(crate) async fn send<S: ChainDataSource, B>(
    view: &ChainView<S>,
    body: B,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let raw: service::RawTransaction =
        wire::decode_request_within(body, wire::request_limit::TRANSACTION).await?;
    send_reply(view.submit(raw.data.to_vec()).await).map(|reply| unary_response(frame(&reply)))
}

/// Submission outcome as lightwalletd answers it
///
/// - accepted: `sendrawtransaction`'s raw JSON result = quoted display-order txid (lightwalletd
///   relays it untouched)
/// - rejected: gRPC OK, the validator's code + message verbatim (zingolib substring-matches it)
fn send_reply(outcome: Result<TxId, SubmitError>) -> Result<service::SendResponse, Status> {
    match outcome {
        Ok(txid) => {
            Ok(service::SendResponse { error_code: 0, error_message: format!("\"{txid}\"") })
        }
        Err(SubmitError::Rejected(rejection)) => Ok(service::SendResponse {
            error_code: rejection.code(),
            error_message: rejection.message().to_owned(),
        }),
        Err(unreachable @ SubmitError::Unreachable { .. }) => {
            Err(Status::unavailable(unreachable.to_string()))
        }
    }
}

/// `GetTransaction`: `hash` arm only (`TxFilter`'s `(block, index)` = positional, no index maps it)
pub(crate) async fn transaction<S: ChainDataSource, B>(
    validators: &TrafficBalancer<S>,
    body: B,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let filter: service::TxFilter = decode_request(body).await?;
    let txid: [u8; 32] = filter
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| Status::invalid_argument("txid must be 32 bytes, in protocol order"))?;
    let found = raw_transaction(validators, TxId::from_bytes(txid)).await?;
    Ok(unary_response(frame(&found)))
}

/// Consensus bytes from whichever validator holds it (`GetTransaction`, `GetTaddressTransactions`
/// bytes)
///
/// - `height` = mined height, or `0` = the wire's "in the mempool" (= no-height location)
/// - `NOT_FOUND` only when every validator asked said absent (a failure = maybe held)
pub(super) async fn raw_transaction<S: ChainDataSource>(
    validators: &TrafficBalancer<S>,
    txid: TxId,
) -> Result<RawTransaction, Status> {
    let found =
        validators.transaction(txid).await.map_err(|unanswered| match &unanswered.last {
            Some(QueryError::Domain(GetTransactionError::NotFound(txid))) => {
                Status::not_found(format!("transaction not found: {txid}"))
            }
            _ => Status::unavailable(unanswered.to_string()),
        })?;
    let found = found.value;

    // Orphaned → unmined (one wire "no height", an abandoned branch must not read as confirmed)
    let height = match found.location {
        TransactionLocation::BestChain(at) => at.into(),
        TransactionLocation::NonBestChain | TransactionLocation::Mempool => 0,
    };

    Ok(RawTransaction { data: found.bytes.into(), height })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - Accepted = code 0 + quoted display-order txid (`sendrawtransaction`'s JSON result)
    /// - Rejected = gRPC OK + the validator's code + message verbatim (zingolib's
    ///   `classify_rejection` texts intact, Android reconciles only on an OK reply)
    /// - Unreachable = `UNAVAILABLE` (never a reply a wallet reads as the network's answer)
    #[test]
    fn a_submission_answers_like_lightwalletd() {
        use zaino_source::SendRawTransactionError::{Malformed, Rejected};

        let mut internal = [0u8; 32];
        internal[0] = 0x01;
        internal[31] = 0xff;
        let accepted = send_reply(Ok(TxId::from_bytes(internal))).expect("a reply");
        let quoted_display = format!("\"ff{}01\"", "00".repeat(30));
        assert_eq!((accepted.error_code, accepted.error_message), (0, quoted_display));

        let verdicts = [
            (-1, "transaction already exists in mempool"),
            (-27, "transaction already in block chain"),
            (-1, "transaction dropped because it is already queued for download"),
            (-25, "bad-txns-inputs-spent"),
        ];
        for (code, message) in verdicts {
            let rejection = Rejected { code, message: message.to_owned() };
            let reply = send_reply(Err(SubmitError::Rejected(rejection))).expect("a reply");
            assert_eq!((reply.error_code, reply.error_message.as_str()), (code, message));
        }
        let malformed = SubmitError::Rejected(Malformed("tx unparseable".into()));
        let reply = send_reply(Err(malformed)).expect("a reply");
        assert_eq!((reply.error_code, reply.error_message.as_str()), (-22, "tx unparseable"));

        let down = zaino_source::NonDomainError::new(zaino_source::FailureMode::Connection, "gone");
        let unreachable = send_reply(Err(SubmitError::Unreachable { attempted: 2, cause: down }));
        let status = unreachable.expect_err("a status, never a reply");
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }
}
