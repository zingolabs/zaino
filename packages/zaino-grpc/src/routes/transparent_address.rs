//! Transparent-address methods: utxos and balances from the index, `GetTaddressTransactions`
//! (index names the txids, a validator supplies the bytes), all as of the snapshot's tip

use bytes::Bytes;
use zaino_index_transparent_address::{AddressUtxo, ServeError, TransparentAddressReader};
use zaino_persistence::{LayeredView, MapRead};
use zaino_primitives::network::network_name;
use zaino_primitives::types::Zatoshis;
use zaino_proto::proto::service as proto;
use zaino_source::{ChainDataSource, TrafficBalancer};
use zcash_address::{ConversionError, ZcashAddress};
use zcash_protocol::consensus::NetworkType;
use zcash_script::script::Evaluable;
use zcash_transparent::address::TransparentAddress;

use http::{HeaderValue, Response};
use http_body::Frame;
use http_body_util::StreamBody;
use tonic::{body::Body, Status};

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::wire::{
    self, decode_request, frame, path, status_response, streamed_response, trailers, unary_response,
};

/// One request's index: the snapshot's reader (as of its tip, row-capped) + the network its
/// addresses parse in
pub(crate) struct Addresses<V> {
    pub(crate) reader: TransparentAddressReader<LayeredView<V>>,
    pub(crate) network: NetworkType,
}

fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::SupplyExceeded => Status::internal(error.to_string()),
        ServeError::TooManyRows { .. } => Status::resource_exhausted(error.to_string()),
    }
}

pub(crate) async fn dispatch<V: MapRead, B>(
    index: Addresses<V>,
    path: &str,
    body: B,
    reads: ReadLanes,
) -> Response<Body>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let answer = match path {
        path::GET_ADDRESS_UTXOS => utxos(index, body, &reads)
            .await
            .map(|address_utxos| wire::frame(&proto::GetAddressUtxosReplyList { address_utxos }))
            .map(unary_response),
        path::GET_ADDRESS_UTXOS_STREAM => utxos(index, body, &reads)
            .await
            .map(|replies| replies.iter().map(wire::frame).collect())
            .map(streamed_response),
        path::GET_TADDRESS_BALANCE => balance_of(index, body, &reads).await.map(unary_response),
        path::GET_TADDRESS_BALANCE_STREAM => {
            streamed_balance_of(index, body, &reads).await.map(unary_response)
        }
        _ => Err(Status::unimplemented("not a transparent-address method")),
    };

    match answer {
        Ok(response) => response,
        Err(status) => status_response(status),
    }
}

/// `GetTaddressTransactions`: index names them, a validator supplies them
///
/// - Fetched lazily, one per poll (busy address = thousands; a client that stops reading stops
///   the round trips; HTTP/2 flow control paces)
pub(crate) async fn transactions<S: ChainDataSource, V: MapRead, B>(
    index: Addresses<V>,
    validators: TrafficBalancer<S>,
    body: B,
    reads: ReadLanes,
) -> Response<Body>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let found = match found_transactions(index, body, &reads).await {
        Ok(found) => found,
        Err(status) => return status_response(status),
    };

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

    let mut response = Response::new(Body::new(StreamBody::new(frames)));
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

    response
}

/// Txids touching the address, height order
async fn found_transactions<V: MapRead, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Vec<zaino_primitives::types::TransactionId>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::TransparentAddressBlockFilter = decode_request(body).await?;
    let address = transparent_address(&request.address, index.network)?;
    let range = request.range.ok_or_else(|| Status::invalid_argument("range is required"))?;

    let height = |bound: Option<proto::BlockId>, field: &str| {
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
async fn utxos<V: MapRead, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Vec<proto::GetAddressUtxosReply>, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::GetAddressUtxosArg = wire::decode_request(body).await?;
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
) -> Result<proto::GetAddressUtxosReply, Status> {
    Ok(proto::GetAddressUtxosReply {
        address: encoded.to_owned(),
        txid: <[u8; 32]>::from(utxo.outpoint.txid).to_vec(),
        index: i32::try_from(utxo.outpoint.vout)
            .map_err(|_| Status::internal("stored vout is above the protocol ceiling"))?,
        script: address.script().to_bytes(),
        value_zat: utxo.value.as_i64(),
        height: u64::from(utxo.height),
    })
}

async fn balance_of<V: MapRead, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Bytes, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let list: proto::AddressList = wire::decode_request(body).await?;
    balance(index, &list.addresses, reads).await
}

/// `GetTaddressBalanceStream`: client-streaming (one framed `Address` each), reply = one total
async fn streamed_balance_of<V: MapRead, B>(
    index: Addresses<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Bytes, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let streamed: Vec<proto::Address> = wire::decode_request_stream(body).await?;
    let addresses: Vec<String> = streamed.into_iter().map(|one| one.address).collect();

    balance(index, &addresses, reads).await
}

/// Deduped on the parsed address (a repeated address counts once)
/// - distinct addresses' balances sum within the supply, so overflow = index corruption
async fn balance<V: MapRead>(
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

    let total = Zatoshis::sum_balances(balances.map_err(to_status)?.into_iter())
        .expect("distinct addresses' balances exceed the money supply: index corrupt");

    Ok(wire::frame(&proto::Balance { value_zat: total.as_i64() }))
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, Request, Response};
    use http_body_util::Full;
    use tonic::{body::Body, Status};

    use zaino_persistence::IndexKind;
    use zaino_proto::frame::{frame_into, FRAME_HEADER};

    use crate::service::Routes;
    use crate::testing::{dispatch, framed_request, indexed, routes, routes_over, snapshot};
    use crate::wire::path;

    /// - All four methods off the same rows: utxo shapes agree, balance shapes agree
    /// - Unparseable address = bad request, never an empty answer (a gap-limit walk would read
    ///   "no history")
    #[tokio::test]
    async fn a_populated_transparent_index_answers_utxos_and_balances_in_both_shapes() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{
            Script, Transaction, TransactionId, TransparentData, TransparentOutput, Zatoshis,
        };
        use zaino_proto::proto::service as proto;
        use zaino_source::mock::MockChain;

        // `t1Hsc…` = hash160 `00…00`, `t3Mg6…` = p2sh `22…22` (base58check, mainnet prefixes)
        const ALICE: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        const BOB: &str = "t3Mg6o2UpMFVtrzqGs7f2VTS6DaiPnFT5rL";
        let alice_script = [&[0x76, 0xa9, 0x14][..], &[0x00; 20], &[0x88, 0xac]].concat();
        let bob_script = [&[0xa9, 0x14][..], &[0x22; 20], &[0x87]].concat();

        // 0: alice 500 (vout 0), bob 70 (vout 1); 1: alice 300
        let pays = |tag: u8, outputs: Vec<(Vec<u8>, u64)>| Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: Vec::new(),
                outputs: outputs
                    .into_iter()
                    .map(|(script, value)| TransparentOutput {
                        value: Zatoshis::new(value).expect("in supply"),
                        script: Script::new(script),
                    })
                    .collect(),
            },
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        let genesis = pays(0x10, vec![(alice_script.clone(), 500), (bob_script.clone(), 70)]);
        let mut chain = Chain::with_genesis(vec![genesis]);
        let tip = chain
            .mine_with(chain.genesis().hash, vec![pays(0x11, vec![(alice_script.clone(), 300)])]);
        let blocks = chain.path(tip.hash);
        let index = indexed(IndexKind::TransparentAddress, &blocks);

        // the validator serves the same chain the index holds
        let node = std::sync::Arc::new(MockChain::serving(chain.path(tip.hash)));
        let routes = Routes { nfs: snapshot(&blocks, vec![index]), ..routes_over(&node).0 };
        let mut router = dispatch(routes);

        async fn body_of(response: Response<Body>) -> bytes::Bytes {
            use http_body_util::BodyExt as _;
            response.into_body().collect().await.expect("body").to_bytes()
        }

        async fn drained(response: Response<Body>) -> (Vec<bytes::Bytes>, HeaderMap) {
            use http_body_util::BodyExt as _;

            let mut body = std::pin::pin!(response.into_body());
            let mut chunks = Vec::new();
            let mut trailers = None;

            while let Some(frame) = body.frame().await {
                let frame = frame.expect("frame");
                assert!(trailers.is_none(), "trailers are the last frame");
                match frame.into_data() {
                    Ok(chunk) => chunks.push(chunk),
                    Err(frame) => trailers = frame.into_trailers().ok(),
                }
            }

            (chunks, trailers.expect("a streaming body ends in trailers"))
        }

        // GetAddressUtxos: one list message, height-ordered, script + address rebuilt
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let list =
            proto::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        let alice_utxos = vec![
            proto::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: vec![0x10; 32],
                index: 0,
                script: alice_script.clone(),
                value_zat: 500,
                height: 0,
            },
            proto::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: vec![0x11; 32],
                index: 0,
                script: alice_script.clone(),
                value_zat: 300,
                height: 1,
            },
        ];
        assert_eq!(list.address_utxos, alice_utxos);

        // maxEntries caps the list (oldest kept)
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
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
            proto::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        assert_eq!(capped.address_utxos, list.address_utxos[..1]);

        // GetAddressUtxosStream: same records, one framed reply per data frame
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS_STREAM,
                proto::GetAddressUtxosArg {
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
        let reply = |chunk: &bytes::Bytes| {
            proto::GetAddressUtxosReply::decode(&chunk[FRAME_HEADER..]).expect("framed reply")
        };
        let streamed: Vec<_> = chunks.iter().map(reply).collect();
        assert_eq!(streamed, list.address_utxos, "one reply per record, same as the list shape");

        // GetTaddressBalance, both addresses (alice repeated: counted once) + client-streaming
        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                proto::AddressList {
                    addresses: vec![ALICE.to_owned(), BOB.to_owned(), ALICE.to_owned()],
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let balance = proto::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), proto::Balance { value_zat: 870 });

        let mut streamed = Vec::new();
        for address in [ALICE, BOB] {
            let message = proto::Address { address: address.to_owned() };
            frame_into(&mut streamed, |out| message.encode_raw(out));
        }
        let response = router
            .call(
                Request::builder()
                    .uri(format!("http://localhost{}", path::GET_TADDRESS_BALANCE_STREAM))
                    .body(Full::new(bytes::Bytes::from(streamed)))
                    .expect("request"),
            )
            .await
            .expect("router answers");
        let balance = proto::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), proto::Balance { value_zat: 870 }, "= list shape");

        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                proto::AddressList { addresses: vec!["not-an-address".to_owned()] }
                    .encode_to_vec()
                    .into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("3")), "invalid argument");

        // GetTaddressTransactions: index names the txids, a validator supplies the bytes
        // (`MockChain`: a mined tx's body = its txid, height = its best-chain block)
        let filter = proto::TransparentAddressBlockFilter {
            address: ALICE.to_owned(),
            range: Some(proto::BlockRange {
                start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
                pool_types: Vec::new(),
            }),
        };

        // TODO: REMOVE the `GetTaddressTxids` half with the deprecated alias
        let mut answers = Vec::new();
        for path in [path::GET_TADDRESS_TRANSACTIONS, path::GET_TADDRESS_TXIDS] {
            let (chunks, trailing) = drained(
                router
                    .call(framed_request(path, filter.encode_to_vec().into()))
                    .await
                    .expect("router answers"),
            )
            .await;
            assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")), "{path}");
            let txs: Vec<proto::RawTransaction> = chunks
                .iter()
                .map(|chunk| proto::RawTransaction::decode(&chunk[FRAME_HEADER..]).expect("tx"))
                .collect();
            answers.push(txs);
        }
        let alice_txs = vec![
            proto::RawTransaction { data: vec![0x10; 32].into(), height: 0 },
            proto::RawTransaction { data: vec![0x11; 32].into(), height: 1 },
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
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{
            Script, Transaction, TransactionId, TransparentData, TransparentOutput, Zatoshis,
        };
        use zaino_proto::proto::service as proto;
        use zcash_address::ToAddress as _;
        use zcash_protocol::consensus::NetworkType;

        let chain = Chain::with_genesis(vec![Transaction {
            txid: TransactionId::from([0x10; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: Vec::new(),
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(500).expect("in supply"),
                    script: Script::new(
                        [&[0x76, 0xa9, 0x14][..], &[0x00; 20], &[0x88, 0xac]].concat(),
                    ),
                }],
            },
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }]);
        let blocks = chain.path(chain.genesis().hash);
        let index = indexed(IndexKind::TransparentAddress, &blocks);
        let mut router = dispatch(Routes { nfs: snapshot(&blocks, vec![index]), ..routes() });

        let mainnet = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        let testnet =
            zcash_address::ZcashAddress::from_transparent_p2pkh(NetworkType::Test, [0x00; 20])
                .encode();
        let every_method = |address: &str| {
            let range = Some(proto::BlockRange {
                start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                end: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                pool_types: Vec::new(),
            });
            [
                (
                    path::GET_TADDRESS_BALANCE,
                    proto::AddressList { addresses: vec![address.to_owned()] }.encode_to_vec(),
                ),
                (
                    path::GET_ADDRESS_UTXOS,
                    proto::GetAddressUtxosArg {
                        addresses: vec![address.to_owned()],
                        start_height: 0,
                        max_entries: 0,
                    }
                    .encode_to_vec(),
                ),
                (
                    path::GET_TADDRESS_TRANSACTIONS,
                    proto::TransparentAddressBlockFilter { address: address.to_owned(), range }
                        .encode_to_vec(),
                ),
            ]
        };

        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                every_method(mainnet)[0].1.clone().into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("0")), "own network's encoding answers");

        for (path, request) in every_method(&testnet) {
            let response =
                router.call(framed_request(path, request.into())).await.expect("router answers");
            let status = Status::from_header_map(response.headers()).expect("a status header");
            let named = format!("address {testnet}: a testnet address, and this index is mainnet");
            let got = (status.code(), status.message());
            assert_eq!(got, (tonic::Code::InvalidArgument, named.as_str()), "{path}");
        }
    }
}
