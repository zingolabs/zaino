//! Transparent-address methods: utxos and balances from the index, `GetTaddressTransactions`
//! (index names the txids, a validator supplies the bytes), all as of the snapshot's tip

use crate::network::network_name;
use bytes::Bytes;
use zaino_index_transparent_address::{AddressUtxo, ServeError, TransparentAddressReader};
use zaino_persistence::{CommittedView, OverlayView};
use zaino_proto::proto::service;
use zaino_source::ChainDataSource;
use zaino_traffic::TrafficBalancer;
use zcash_address::{ConversionError, ZcashAddress};
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_script::script::Evaluable;
use zcash_transparent::address::TransparentAddress;

use http::Response;
use http_body::Frame;
use tonic::{body::Body, Status};

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::wire::{self, decode_request, frame, streamed_response, trailers, unary_response};

/// One request's index: the snapshot's reader (as of its tip, row-capped) + the network its
/// addresses parse in
pub(crate) struct Addresses<V> {
    pub(crate) reader: TransparentAddressReader<OverlayView<V>>,
    pub(crate) network: NetworkType,
}

fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::SupplyExceeded => Status::internal(error.to_string()),
        ServeError::TooManyRows { .. } => Status::resource_exhausted(error.to_string()),
    }
}

/// `GetAddressUtxos`: one list message
pub(crate) async fn utxos<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let address_utxos = utxo_replies(index, body, reads).await?;
    Ok(unary_response(wire::frame(&service::GetAddressUtxosReplyList { address_utxos })))
}

/// `GetAddressUtxosStream`: one message per utxo
pub(crate) async fn utxos_stream<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let replies = utxo_replies(index, body, reads).await?;
    Ok(streamed_response(replies.iter().map(wire::frame).collect()))
}

/// `GetTaddressTransactions`: index names them, a validator supplies them
///
/// - Fetched lazily, one per poll (busy address = thousands; a client that stops reading stops
///   the round trips; HTTP/2 flow control paces)
pub(crate) async fn transactions<S: ChainDataSource, V: CommittedView, B>(
    index: Addresses<V>,
    validators: TrafficBalancer<S>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let found = found_transactions(index, body, reads).await?;

    // `None` state ends the stream (trailer frame always last, always sent)
    let frames = futures::stream::unfold(Some(found.into_iter()), move |state| {
        let validators = validators.clone();
        async move {
            let mut rest = state?;

            let Some(txid) = rest.next() else {
                return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
            };

            let fetched = super::chain::raw_transaction(&validators, txid).await;
            match fetched.map(|tx| frame(&tx)) {
                Ok(record) => Some((Ok::<_, Status>(Frame::data(record)), Some(rest))),
                Err(status) => Some((Ok(Frame::trailers(trailers(&status))), None)),
            }
        }
    });

    Ok(wire::streaming(frames))
}

/// Txids touching the address, height order
async fn found_transactions<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Vec<zcash_protocol::TxId>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: service::TransparentAddressBlockFilter = decode_request(body).await?;
    let address = transparent_address(&request.address, index.network)?;
    let range = request.range.ok_or_else(|| Status::invalid_argument("range is required"))?;

    let height = |bound: Option<service::BlockId>, field: &str| {
        bound
            .ok_or_else(|| Status::invalid_argument(format!("range.{field} is required")))
            .and_then(|at| wire::height(at.height, field))
    };
    let (start, end) = wire::ordered(height(range.start, "start")?, height(range.end, "end")?)?;

    let found =
        reads.read(Lane::Scan, move || index.reader.transactions(&address, start, end)).await?;

    Ok(found.map_err(to_status)?.into_iter().map(|found| found.txid).collect())
}

/// Only `network`'s encodings (index keys = `[kind][hash160]`: a foreign encoding would
/// answer from this network's rows)
fn transparent_address(encoded: &str, network: NetworkType) -> Result<TransparentAddress, Status> {
    let invalid = |reason: String| Status::invalid_argument(format!("address {encoded}: {reason}"));

    encoded
        .parse::<ZcashAddress>()
        .map_err(|error| invalid(error.to_string()))?
        .convert_if_network::<TransparentAddress>(network)
        .map_err(|error| match error {
            ConversionError::IncorrectNetwork { expected, actual } => invalid(format!(
                "a {} address, and this index is {}",
                network_name(actual),
                network_name(expected)
            )),
            _ => invalid("not a transparent address".to_owned()),
        })
}

/// `GetAddressUtxos` / `GetAddressUtxosStream`: one walk, two response shapes
async fn utxo_replies<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Vec<service::GetAddressUtxosReply>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: service::GetAddressUtxosArg = wire::decode_request(body).await?;
    let from = wire::height(request.start_height, "startHeight")?;
    let addresses = request
        .addresses
        .iter()
        .map(|encoded| transparent_address(encoded, index.network))
        .collect::<Result<Vec<_>, _>>()?;

    let (addresses, utxos) = reads
        .read(Lane::Scan, move || {
            let utxos = index.reader.utxos_of(&addresses, from);
            (addresses, utxos)
        })
        .await?;
    let mut found = Vec::new();
    for ((encoded, address), utxos) in
        request.addresses.iter().zip(&addresses).zip(utxos.map_err(to_status)?)
    {
        for utxo in &utxos {
            found.push(reply(encoded, address, utxo)?);
        }
    }

    // `GetAddressUtxosArg` = height-ordered results (a per-address walk is, per address only)
    found.sort_by(|left, right| {
        (left.height, &left.txid, left.index).cmp(&(right.height, &right.txid, right.index))
    });
    if request.max_entries > 0 {
        found.truncate(request.max_entries as usize);
    }

    Ok(found)
}

fn reply(
    encoded: &str,
    address: &TransparentAddress,
    utxo: &AddressUtxo,
) -> Result<service::GetAddressUtxosReply, Status> {
    Ok(service::GetAddressUtxosReply {
        address: encoded.to_owned(),
        txid: utxo.outpoint.hash().to_vec(),
        index: i32::try_from(utxo.outpoint.n())
            .map_err(|_| Status::internal("stored vout is above the protocol ceiling"))?,
        script: address.script().to_bytes(),
        value_zat: i64::from(ZatBalance::from(utxo.value)),
        height: u64::from(utxo.height),
    })
}

/// `GetTaddressBalance`: one `AddressList`, one total
pub(crate) async fn balance<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let list: service::AddressList = wire::decode_request(body).await?;
    total(index, &list.addresses, reads).await.map(unary_response)
}

/// `GetTaddressBalanceStream`: client-streaming (one framed `Address` each), reply = one total
pub(crate) async fn balance_stream<V: CommittedView, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let streamed: Vec<service::Address> = wire::decode_request_stream(body).await?;
    let addresses: Vec<String> = streamed.into_iter().map(|one| one.address).collect();

    total(index, &addresses, reads).await.map(unary_response)
}

/// Deduped on the parsed address (a repeated address counts once)
/// - distinct addresses' balances sum within the supply, so overflow = index corruption
async fn total<V: CommittedView>(
    index: Addresses<V>,
    addresses: &[String],
    reads: &ReadLanes,
) -> Result<Bytes, Status> {
    let distinct: Vec<TransparentAddress> = addresses
        .iter()
        .map(|encoded| transparent_address(encoded, index.network))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?
        .into_iter()
        .collect();
    let balances = reads.read(Lane::Scan, move || index.reader.balances(&distinct)).await?;

    let total = (balances.map_err(to_status)?.into_iter().sum::<Option<Zatoshis>>())
        .expect("distinct addresses' balances exceed the money supply: index corrupt");

    Ok(wire::frame(&service::Balance { value_zat: i64::from(ZatBalance::from(total)) }))
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Request, Response};
    use http_body_util::Full;
    use tonic::{body::Body, Status};

    use zaino_persistence::IndexKind;
    use zaino_proto::frame::{frame_into, FRAME_HEADER};

    use crate::service::{Method, Routes};
    use crate::testing::{
        decode_frames, dispatch, drained, framed_request, indexed, routes, routes_over, snapshot,
    };

    /// - All four methods off the same rows: utxo shapes agree, balance shapes agree
    /// - Unparseable address = bad request, never an empty answer (a gap-limit walk would read
    ///   "no history")
    #[tokio::test]
    async fn a_populated_transparent_index_answers_utxos_and_balances_in_both_shapes() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::{p2pkh, MockChain};
        use zaino_proto::proto::service;
        use zaino_source::testing::{decoded, raw_transparent, ValidatorViews};
        use zcash_script::script::Code;
        use zcash_transparent::address::Script;
        use zcash_transparent::bundle::OutPoint;

        // `t1Hsc…` = hash160 `00…00`, `t3Mg6…` = p2sh `22…22` (base58check, mainnet prefixes)
        const ALICE: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        const BOB: &str = "t3Mg6o2UpMFVtrzqGs7f2VTS6DaiPnFT5rL";
        let alice = p2pkh([0x00; 20]);
        let bob = Script(Code([&[0xa9, 0x14][..], &[0x22; 20], &[0x87]].concat()));
        let miner = p2pkh([0x33; 20]);

        // 1: a coinbase funds the miner 870; 2: alice 500 (vout 0), bob 70 (vout 1), change 300
        // (vout 2); 3: the change to alice (real bytes: a validator serves them)
        let mut chain = MockChain::regtest().network(crate::testing::MAINNET);
        chain.mine(|b| b.coinbase(|c| c.txid([0x01; 32]).pay(&miner, 870)));
        let pays = [(&alice, 500), (&bob, 70), (&miner, 300)];
        let (first, first_raw) = raw_transparent(&[OutPoint::new([0x01; 32], 0)], &pays);
        chain.mine(|b| b.raw_tx(decoded(first_raw.clone())));
        let change = OutPoint::new(first.into(), 2);
        let (second, second_raw) = raw_transparent(&[change], &[(&alice, 300)]);
        let tip = chain.mine(|b| b.raw_tx(decoded(second_raw.clone())));
        let index = indexed(IndexKind::TransparentAddress, &chain, tip);

        // the validator serves the same chain the index holds
        let node = std::sync::Arc::new(chain.validator(tip));
        let (routes, balancing, _) = routes_over(&node);
        let routes = Routes { snapshots: snapshot(&chain, tip, vec![index]), ..routes };
        tokio::spawn(balancing.run(tokio_util::sync::CancellationToken::new()));
        let mut router = dispatch(routes);

        // A unary answer: data, then `OK` trailers (status in headers beside a body = malformed)
        async fn body_of(response: Response<Body>) -> bytes::Bytes {
            assert_eq!(response.headers().get("grpc-status"), None, "unary status in headers");
            let (chunks, trailing) = drained(response).await;
            assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")), "unary");
            chunks.concat().into()
        }

        // GetAddressUtxos: one list message, height-ordered, script + address rebuilt
        let response = router
            .call(framed_request(
                Method::GetAddressUtxos,
                service::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let list =
            service::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        let alice_utxos = vec![
            service::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: first.as_ref().to_vec(),
                index: 0,
                script: alice.0 .0.clone(),
                value_zat: 500,
                height: 2,
            },
            service::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: second.as_ref().to_vec(),
                index: 0,
                script: alice.0 .0.clone(),
                value_zat: 300,
                height: 3,
            },
        ];
        assert_eq!(list.address_utxos, alice_utxos);

        // maxEntries caps the list (oldest kept)
        let response = router
            .call(framed_request(
                Method::GetAddressUtxos,
                service::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 1,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let capped =
            service::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        assert_eq!(capped.address_utxos, list.address_utxos[..1]);

        // GetAddressUtxosStream: same records, one framed reply each (ready replies joined into
        // one data frame: lightwalletd #593)
        let response = router
            .call(framed_request(
                Method::GetAddressUtxosStream,
                service::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, None, "stream status rides in the trailers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        assert_eq!(chunks.len(), 1, "two ready replies, one data frame");
        let streamed = decode_frames::<service::GetAddressUtxosReply>(&chunks.concat());
        assert_eq!(streamed, list.address_utxos, "one reply per record, same as the list shape");

        // GetTaddressBalance, both addresses (alice repeated: counted once) + client-streaming
        let response = router
            .call(framed_request(
                Method::GetTaddressBalance,
                service::AddressList {
                    addresses: vec![ALICE.to_owned(), BOB.to_owned(), ALICE.to_owned()],
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let balance = service::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), service::Balance { value_zat: 870 });

        let mut streamed = Vec::new();
        for address in [ALICE, BOB] {
            let message = service::Address { address: address.to_owned() };
            frame_into(&mut streamed, |out| message.encode_raw(out));
        }
        let response = router
            .call(
                Request::builder()
                    .uri(format!("http://localhost{}", Method::GetTaddressBalanceStream.path()))
                    .body(Full::new(bytes::Bytes::from(streamed)))
                    .expect("request"),
            )
            .await
            .expect("router answers");
        let balance = service::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), service::Balance { value_zat: 870 }, "= list shape");

        let response = router
            .call(framed_request(
                Method::GetTaddressBalance,
                service::AddressList { addresses: vec!["not-an-address".to_owned()] }
                    .encode_to_vec()
                    .into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("3")), "invalid argument");

        // GetTaddressTransactions: index names the txids, a validator supplies the bytes + height
        let filter = service::TransparentAddressBlockFilter {
            address: ALICE.to_owned(),
            range: Some(service::BlockRange {
                start: Some(service::BlockId { height: 0, hash: Vec::new() }),
                end: Some(service::BlockId { height: 3, hash: Vec::new() }),
                pool_types: Vec::new(),
            }),
        };

        // TODO: REMOVE the `GetTaddressTxids` half with the deprecated alias
        let mut answers = Vec::new();
        for method in [Method::GetTaddressTransactions, Method::GetTaddressTxids] {
            let (chunks, trailing) = drained(
                router
                    .call(framed_request(method, filter.encode_to_vec().into()))
                    .await
                    .expect("router answers"),
            )
            .await;
            let ok = Some(&HeaderValue::from_static("0"));
            assert_eq!(trailing.get("grpc-status"), ok, "{method:?}");
            answers.push(decode_frames::<service::RawTransaction>(&chunks.concat()));
        }
        let alice_txs = vec![
            service::RawTransaction { data: first_raw.into(), height: 2 },
            service::RawTransaction { data: second_raw.into(), height: 3 },
        ];
        assert_eq!(answers[0], alice_txs, "both of alice's, height order, validator bytes");
        assert_eq!(answers[0], answers[1], "the deprecated alias answers identically");
    }

    /// Foreign-network encoding of a funded hash160 → `INVALID_ARGUMENT` naming both networks,
    /// on every transparent method (never this network's rows under another network's address)
    #[tokio::test]
    async fn a_foreign_network_address_is_refused_by_every_transparent_method() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::{p2pkh, MockChain};
        use zaino_proto::proto::service;
        use zcash_address::ToAddress as _;
        use zcash_protocol::consensus::NetworkType;

        let alice = p2pkh([0x00; 20]);
        let chain = MockChain::regtest()
            .network(crate::testing::MAINNET)
            .genesis_with(|b| b.coinbase(|c| c.pay(&alice, 500)));
        let genesis = chain.genesis();
        let index = indexed(IndexKind::TransparentAddress, &chain, genesis);
        let snapshots = snapshot(&chain, genesis, vec![index]);
        let mut router = dispatch(Routes { snapshots, ..routes() });

        let mainnet = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        let testnet =
            zcash_address::ZcashAddress::from_transparent_p2pkh(NetworkType::Test, [0x00; 20])
                .encode();
        let every_method = |address: &str| {
            let range = Some(service::BlockRange {
                start: Some(service::BlockId { height: 0, hash: Vec::new() }),
                end: Some(service::BlockId { height: 0, hash: Vec::new() }),
                pool_types: Vec::new(),
            });
            [
                (
                    Method::GetTaddressBalance,
                    service::AddressList { addresses: vec![address.to_owned()] }.encode_to_vec(),
                ),
                (
                    Method::GetAddressUtxos,
                    service::GetAddressUtxosArg {
                        addresses: vec![address.to_owned()],
                        start_height: 0,
                        max_entries: 0,
                    }
                    .encode_to_vec(),
                ),
                (
                    Method::GetTaddressTransactions,
                    service::TransparentAddressBlockFilter { address: address.to_owned(), range }
                        .encode_to_vec(),
                ),
            ]
        };

        let response = router
            .call(framed_request(
                Method::GetTaddressBalance,
                every_method(mainnet)[0].1.clone().into(),
            ))
            .await
            .expect("router answers");
        let body = http_body_util::BodyExt::collect(response.into_body()).await.expect("body");
        let status = body.trailers().and_then(|trailers| trailers.get("grpc-status").cloned());
        assert_eq!(status, Some(HeaderValue::from_static("0")), "own network's encoding answers");

        for (method, request) in every_method(&testnet) {
            let response =
                router.call(framed_request(method, request.into())).await.expect("router answers");
            let status = Status::from_header_map(response.headers()).expect("a status header");
            let named = format!("address {testnet}: a testnet address, and this index is mainnet");
            let got = (status.code(), status.message());
            assert_eq!(got, (tonic::Code::InvalidArgument, named.as_str()), "{method:?}");
        }
    }
}
