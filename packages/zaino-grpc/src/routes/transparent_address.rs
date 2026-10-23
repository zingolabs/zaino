//! Transparent-address methods: utxos and balances from the index, `GetTaddressTransactions`
//! (index names the txids, a validator supplies the bytes), all as of the served tip

use std::sync::Arc;

use zaino_grpc_snapshot::GrpcSnapshot;
use zaino_index_transparent_address::{AddressUtxo, ServeError, TransparentAddressReader};
use zaino_persistence::{OverlayView, PinnedView};
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

use super::args;
use super::info::chain_name;
use crate::read_lanes::{Lane, ReadLanes};
use crate::wire::{self, frame, streamed_response, trailers, unary_response};

/// One request's transparent-address index: the reader (as of the served tip, row-capped) + the
/// network its addresses parse in
pub(crate) struct AddressIndex<V> {
    pub(crate) reader: TransparentAddressReader<OverlayView<V>>,
    pub(crate) network: NetworkType,
}

/// `GetAddressUtxos`: every utxo in one list message
pub(crate) async fn get_address_utxos<V: PinnedView>(
    address_index: AddressIndex<V>,
    request: service::GetAddressUtxosArg,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let address_utxos = address_utxos(address_index, &request, reads).await?;
    let reply = service::GetAddressUtxosReplyList { address_utxos };
    Ok(unary_response(wire::frame(&reply)))
}

/// `GetAddressUtxosStream`: one message per utxo
pub(crate) async fn get_address_utxos_stream<V: PinnedView>(
    address_index: AddressIndex<V>,
    request: service::GetAddressUtxosArg,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let address_utxos = address_utxos(address_index, &request, reads).await?;
    Ok(streamed_response(address_utxos.iter().map(wire::frame).collect()))
}

/// `GetTaddressBalance`: one `AddressList`, one total
pub(crate) async fn get_taddress_balance<V: PinnedView>(
    address_index: AddressIndex<V>,
    address_list: service::AddressList,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let balance = total_balance(address_index, &address_list.addresses, reads).await?;
    Ok(unary_response(wire::frame(&balance)))
}

/// `GetTaddressBalanceStream`: client-streaming (one framed `Address` each), one total
pub(crate) async fn get_taddress_balance_stream<V: PinnedView>(
    address_index: AddressIndex<V>,
    streamed: Vec<service::Address>,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let addresses: Vec<String> = streamed.into_iter().map(|one| one.address).collect();

    let balance = total_balance(address_index, &addresses, reads).await?;
    Ok(unary_response(wire::frame(&balance)))
}

/// `GetTaddressTransactions`: the index names the txids, a validator (else the peers' blocks)
/// supplies each one's bytes
pub(crate) async fn get_taddress_transactions<S: ChainDataSource, V: PinnedView>(
    address_index: AddressIndex<V>,
    validators: TrafficBalancer<S>,
    grpc_snapshot: Arc<GrpcSnapshot<V>>,
    filter: service::TransparentAddressBlockFilter,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let txids = address_txids(address_index, filter, reads).await?;
    Ok(streamed_raw_transactions(txids, validators, grpc_snapshot))
}

/// Utxos of every requested address at or above `startHeight`, height order, capped at
/// `maxEntries` (shared by `GetAddressUtxos` and its stream)
async fn address_utxos<V: PinnedView>(
    address_index: AddressIndex<V>,
    request: &service::GetAddressUtxosArg,
    reads: &ReadLanes,
) -> Result<Vec<service::GetAddressUtxosReply>, Status> {
    let from = args::height(request.start_height, "startHeight")?;
    let addresses = request
        .addresses
        .iter()
        .map(|encoded| parse_transparent_address(encoded, address_index.network))
        .collect::<Result<Vec<_>, _>>()?;

    let reader = address_index.reader;
    let (addresses, utxos_per_address) = reads
        .read(Lane::Scan, move || {
            let utxos = reader.utxos_of(&addresses, from);
            (addresses, utxos)
        })
        .await?;
    let utxos_per_address = utxos_per_address.map_err(serve_error_status)?;

    let mut address_utxos = Vec::new();
    for ((encoded, address), utxos) in
        request.addresses.iter().zip(&addresses).zip(utxos_per_address)
    {
        for utxo in &utxos {
            address_utxos.push(address_utxo_message(encoded, address, utxo)?);
        }
    }

    // `GetAddressUtxosArg` = height-ordered results (a per-address walk is, per address only)
    address_utxos.sort_by(|left, right| {
        (left.height, &left.txid, left.index).cmp(&(right.height, &right.txid, right.index))
    });
    if request.max_entries > 0 {
        address_utxos.truncate(request.max_entries as usize);
    }
    Ok(address_utxos)
}

/// Sum over the distinct parsed addresses (a repeated address counts once)
///
/// - distinct addresses' balances sum within the supply: overflow = index corruption
async fn total_balance<V: PinnedView>(
    address_index: AddressIndex<V>,
    addresses: &[String],
    reads: &ReadLanes,
) -> Result<service::Balance, Status> {
    let distinct: Vec<TransparentAddress> = addresses
        .iter()
        .map(|encoded| parse_transparent_address(encoded, address_index.network))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?
        .into_iter()
        .collect();

    let reader = address_index.reader;
    let balances = reads.read(Lane::Scan, move || reader.balances(&distinct)).await?;
    let total = balances.map_err(serve_error_status)?.into_iter().sum::<Option<Zatoshis>>();
    let total = total.ok_or_else(|| {
        Status::internal("distinct addresses' balances exceed the money supply: index corrupt")
    })?;
    Ok(service::Balance { value_zat: i64::from(ZatBalance::from(total)) })
}

/// Txids touching the filter's address within its range, height order
async fn address_txids<V: PinnedView>(
    address_index: AddressIndex<V>,
    filter: service::TransparentAddressBlockFilter,
    reads: &ReadLanes,
) -> Result<Vec<zcash_protocol::TxId>, Status> {
    let address = parse_transparent_address(&filter.address, address_index.network)?;
    let range = filter.range.ok_or_else(|| Status::invalid_argument("range is required"))?;
    let bound = |id: Option<service::BlockId>, field: &str| {
        let id =
            id.ok_or_else(|| Status::invalid_argument(format!("range.{field} is required")))?;
        args::height(id.height, field)
    };
    let (start, end) = args::ordered(bound(range.start, "start")?, bound(range.end, "end")?)?;

    let reader = address_index.reader;
    let touching = reads.read(Lane::Scan, move || reader.transactions(&address, start, end));
    let touching = touching.await?.map_err(serve_error_status)?;
    Ok(touching.into_iter().map(|transaction| transaction.txid).collect())
}

/// One `RawTransaction` per txid, fetched lazily
///
/// - one fetch per poll (busy address = thousands)
/// - client that stops reading stops the round trips (HTTP/2 flow control paces)
/// - trailer frame always last, always sent (`None` state = stream ended)
fn streamed_raw_transactions<S: ChainDataSource, V: PinnedView>(
    txids: Vec<zcash_protocol::TxId>,
    validators: TrafficBalancer<S>,
    grpc_snapshot: Arc<GrpcSnapshot<V>>,
) -> Response<Body> {
    let frames = futures::stream::unfold(Some(txids.into_iter()), move |state| {
        let (validators, grpc_snapshot) = (validators.clone(), Arc::clone(&grpc_snapshot));
        async move {
            let mut remaining = state?;
            let Some(txid) = remaining.next() else {
                return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
            };

            let fetched =
                super::transactions::fetch_raw_transaction(&validators, &grpc_snapshot, txid).await;
            match fetched {
                Ok(transaction) => {
                    Some((Ok::<_, Status>(Frame::data(frame(&transaction))), Some(remaining)))
                }
                Err(status) => Some((Ok(Frame::trailers(trailers(&status))), None)),
            }
        }
    });
    wire::streaming(frames)
}

fn serve_error_status(error: ServeError) -> Status {
    match &error {
        ServeError::SupplyExceeded => Status::internal(error.to_string()),
        ServeError::TooManyRows { .. } => Status::resource_exhausted(error.to_string()),
    }
}

/// Only `network`'s encodings (index keys = `[kind][hash160]`: a foreign encoding would
/// answer from this network's rows)
fn parse_transparent_address(
    encoded: &str,
    network: NetworkType,
) -> Result<TransparentAddress, Status> {
    let invalid = |reason: String| Status::invalid_argument(format!("address {encoded}: {reason}"));

    encoded
        .parse::<ZcashAddress>()
        .map_err(|error| invalid(error.to_string()))?
        .convert_if_network::<TransparentAddress>(network)
        .map_err(|error| match error {
            ConversionError::IncorrectNetwork { expected, actual } => invalid(format!(
                "a {} address, and this index is {}",
                chain_name(actual),
                chain_name(expected)
            )),
            _ => invalid("not a transparent address".to_owned()),
        })
}

/// Domain utxo → wire `GetAddressUtxosReply` (address as the client spelled it)
fn address_utxo_message(
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

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use prost::Message as _;
    use tonic::Code;
    use tower::Service as _;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_persistence::IndexKind;
    use zaino_proto::proto::service;
    use zaino_source::testing::{p2pkh, raw_transparent, MockChain};
    use zcash_transparent::bundle::OutPoint;

    use crate::service::Method;
    use crate::testing::{
        answer, decode_frames, dispatch, fixed_snapshots, framed_request, indexed, routes_over,
        routes_serving, MAINNET,
    };

    // `t1Hsc…` = hash160 `00…00`, `t3Mg6…` = p2sh `22…22` (base58check, mainnet prefixes)
    const ALICE: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
    const BOB: &str = "t3Mg6o2UpMFVtrzqGs7f2VTS6DaiPnFT5rL";

    fn bob_p2sh() -> zcash_transparent::address::Script {
        use zcash_script::script::Code;
        zcash_transparent::address::Script(Code([&[0xa9, 0x14][..], &[0x22; 20], &[0x87]].concat()))
    }

    fn utxos_of(address: &str, max_entries: u32) -> Bytes {
        let addresses = vec![address.to_owned()];
        service::GetAddressUtxosArg { addresses, start_height: 0, max_entries }
            .encode_to_vec()
            .into()
    }

    /// Script + address rebuilt, height order; the stream joins its ready replies into one data
    /// frame (lightwalletd #593)
    #[tokio::test]
    async fn get_address_utxos_lists_an_addresss_unspent_outputs_in_height_order_in_both_shapes() {
        // 1: coinbase funds the miner 870; 2: alice 500 + change 370; 3: the change to alice
        let (alice, miner) = (p2pkh([0x00; 20]), p2pkh([0x33; 20]));
        let mut chain = MockChain::regtest().network(MAINNET);
        let funded = chain.mine(|b| b.coinbase(|c| c.pay(&miner, 870)));
        let (first, first_raw) =
            raw_transparent(&[chain.output(funded, 0, 0)], &[(&alice, 500), (&miner, 370)]);
        chain.mine(|b| b.raw_tx(first_raw));
        let (second, second_raw) =
            raw_transparent(&[OutPoint::new(first.into(), 1)], &[(&alice, 370)]);
        let tip = chain.mine(|b| b.raw_tx(second_raw));
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, tip)];
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let mut router =
            dispatch(routes_serving(MAINNET, &[IndexKind::TransparentAddress], snapshots));
        let mut call = async |method, message| {
            let response = router.call(framed_request(method, message)).await.expect("infallible");
            let in_headers = response.headers().get("grpc-status").cloned();
            let (chunks, status) = answer(response).await;
            assert_eq!(status.code(), Code::Ok, "{method:?}: {status:?}");
            assert_eq!(in_headers, None, "{method:?}: status beside a body in headers = malformed");
            chunks
        };
        let utxo = |txid: zcash_protocol::TxId, value_zat, height| service::GetAddressUtxosReply {
            address: ALICE.to_owned(),
            txid: txid.as_ref().to_vec(),
            index: 0,
            script: alice.0 .0.clone(),
            value_zat,
            height,
        };
        let alices = vec![utxo(first, 500, 2), utxo(second, 370, 3)];

        let listed = call(Method::GetAddressUtxos, utxos_of(ALICE, 0)).await;
        let capped = call(Method::GetAddressUtxos, utxos_of(ALICE, 1)).await;
        let streamed = call(Method::GetAddressUtxosStream, utxos_of(ALICE, 0)).await;

        let list = |chunks: Vec<Bytes>| {
            let lists = decode_frames::<service::GetAddressUtxosReplyList>(&chunks.concat());
            lists.into_iter().map(|list| list.address_utxos).collect::<Vec<_>>()
        };
        assert_eq!(
            list(listed),
            std::slice::from_ref(&alices),
            "GetAddressUtxos: one list message"
        );
        assert_eq!(list(capped), [alices[..1].to_vec()], "maxEntries 1: the oldest kept");
        assert_eq!(streamed.len(), 1, "two ready replies, one data frame: {streamed:?}");
        let streamed = decode_frames::<service::GetAddressUtxosReply>(&streamed.concat());
        assert_eq!(streamed, alices, "GetAddressUtxosStream: one reply per utxo");
    }

    /// Unparseable address = refused, never an empty answer (a gap-limit walk would read "no
    /// history")
    #[tokio::test]
    async fn get_taddress_balance_sums_each_distinct_address_once_in_both_shapes() {
        // 1: coinbase funds the miner 870; 2: alice 500, bob 70, change 300
        let (alice, miner) = (p2pkh([0x00; 20]), p2pkh([0x33; 20]));
        let mut chain = MockChain::regtest().network(MAINNET);
        let funded = chain.mine(|b| b.coinbase(|c| c.pay(&miner, 870)));
        let pays = [(&alice, 500), (&bob_p2sh(), 70), (&miner, 300)];
        let (_, paying) = raw_transparent(&[chain.output(funded, 0, 0)], &pays);
        let tip = chain.mine(|b| b.raw_tx(paying));
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, tip)];
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let mut router =
            dispatch(routes_serving(MAINNET, &[IndexKind::TransparentAddress], snapshots));
        let mut balance = async |request| {
            let (chunks, status) = answer(router.call(request).await.expect("infallible")).await;
            let balances = decode_frames::<service::Balance>(&chunks.concat());
            (status, balances.into_iter().map(|balance| balance.value_zat).collect::<Vec<_>>())
        };
        let list = |addresses: &[&str]| {
            let addresses = addresses.iter().map(|at| (*at).to_owned()).collect();
            framed_request(
                Method::GetTaddressBalance,
                service::AddressList { addresses }.encode_to_vec().into(),
            )
        };
        let mut streamed = Vec::new();
        for address in [ALICE, BOB] {
            let message = service::Address { address: address.to_owned() };
            zaino_proto::frame::frame_into(&mut streamed, |out| message.encode_raw(out));
        }
        let client_stream = || {
            let mut request = http::Request::builder()
                .uri(format!("http://localhost{}", Method::GetTaddressBalanceStream.path()))
                .body(http_body_util::Full::new(Bytes::from(streamed.clone())))
                .expect("request");
            Method::resolve(&mut request);
            request
        };

        let (status, listed) = balance(list(&[ALICE, BOB, ALICE])).await;
        assert_eq!(
            (status.code(), listed),
            (Code::Ok, vec![570]),
            "alice 500 + bob 70, alice once: {status:?}"
        );
        let (status, streamed) = balance(client_stream()).await;
        assert_eq!(
            (status.code(), streamed),
            (Code::Ok, vec![570]),
            "client-streamed alice, bob: {status:?}"
        );

        let (refused, none) = balance(list(&["not-an-address"])).await;
        assert_eq!((refused.code(), none), (Code::InvalidArgument, vec![]), "{refused:?}");
        let names_it = refused.message().starts_with("address not-an-address: ");
        assert!(names_it, "refusal names the address: {:?}", refused.message());
    }

    /// Index names the txids, the validator supplies the bytes + height
    #[tokio::test]
    async fn get_taddress_transactions_streams_each_of_an_addresss_txs_from_the_validator() {
        // 1: coinbase funds the miner 870; 2: alice 500 + change 370; 3: the change to alice
        let (alice, miner) = (p2pkh([0x00; 20]), p2pkh([0x33; 20]));
        let mut chain = MockChain::regtest().network(MAINNET);
        let funded = chain.mine(|b| b.coinbase(|c| c.pay(&miner, 870)));
        let (first, first_raw) =
            raw_transparent(&[chain.output(funded, 0, 0)], &[(&alice, 500), (&miner, 370)]);
        chain.mine(|b| b.raw_tx(first_raw.clone()));
        let (_, second_raw) = raw_transparent(&[OutPoint::new(first.into(), 1)], &[(&alice, 370)]);
        let tip = chain.mine(|b| b.raw_tx(second_raw.clone()));
        let node = chain.validator(tip);
        let indexes = [IndexKind::TransparentAddress];
        let (routes, balancing, _never_polled) = routes_over(&node, MAINNET, &indexes);
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, tip)];
        let grpc_snapshots =
            fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(balancing.run(cancel.child_token()));
        let mut router = dispatch(crate::service::RouteDeps { grpc_snapshots, ..routes });
        let at = |height| Some(service::BlockId { height, hash: Vec::new() });
        let range = Some(service::BlockRange { start: at(0), end: at(3), pool_types: Vec::new() });
        let filter = service::TransparentAddressBlockFilter { address: ALICE.to_owned(), range };

        let alices = vec![
            service::RawTransaction { data: first_raw.into(), height: 2 },
            service::RawTransaction { data: second_raw.into(), height: 3 },
        ];
        // TODO: REMOVE the `GetTaddressTxids` row with the deprecated alias
        for method in [Method::GetTaddressTransactions, Method::GetTaddressTxids] {
            let response = router.call(framed_request(method, filter.encode_to_vec().into())).await;

            let (chunks, status) = answer(response.expect("infallible")).await;
            assert_eq!(status.code(), Code::Ok, "{method:?}: {status:?}");
            let streamed = decode_frames::<service::RawTransaction>(&chunks.concat());
            assert_eq!(streamed, alices, "{method:?}: alice's 2, height order, validator bytes");
        }
        cancel.cancel();
    }

    /// Address the index cannot key → `INVALID_ARGUMENT` naming it on every transparent method,
    /// never an empty answer (a wallet reads that as "never paid")
    ///
    /// - foreign network = a funded hash160's other encoding (never this network's rows)
    #[tokio::test]
    async fn an_address_the_index_cannot_key_is_refused_by_every_transparent_method() {
        use zcash_address::{ToAddress as _, ZcashAddress};
        use zcash_protocol::consensus::NetworkType;

        let alice = p2pkh([0x00; 20]);
        let chain = MockChain::regtest()
            .network(MAINNET)
            .genesis_with(|b| b.coinbase(|c| c.pay(&alice, 500)));
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::TransparentAddress, &chain, genesis)];
        let snapshots =
            fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
        let mut router =
            dispatch(routes_serving(MAINNET, &[IndexKind::TransparentAddress], snapshots));

        let mainnet = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        let unparseable = "t1-not-an-address";
        let unparsed = unparseable.parse::<ZcashAddress>().expect_err("not base58check");
        let sapling = ZcashAddress::from_sapling(NetworkType::Main, [0x00; 43]).encode();
        let testnet = ZcashAddress::from_transparent_p2pkh(NetworkType::Test, [0x00; 20]).encode();
        #[rustfmt::skip]
        let refused = [
            ("unparseable",     unparseable.to_owned(), unparsed.to_string()),
            ("shielded only",   sapling,                "not a transparent address".to_owned()),
            ("foreign network", testnet,                "a test address, and this index is main".to_owned()),
        ];

        let own = service::AddressList { addresses: vec![mainnet.to_owned()] }.encode_to_vec();
        let response = router.call(framed_request(Method::GetTaddressBalance, own.into())).await;
        let (chunks, status) = answer(response.expect("infallible")).await;
        let balances = decode_frames::<service::Balance>(&chunks.concat());
        assert_eq!(balances, [service::Balance { value_zat: 500 }], "own encoding: {status:?}");

        for (case, address, reason) in refused {
            let address_list = service::AddressList { addresses: vec![address.clone()] };
            let one_address = service::Address { address: address.clone() };
            let utxos_arg = service::GetAddressUtxosArg {
                addresses: vec![address.clone()],
                ..Default::default()
            };
            let range = Some(service::BlockRange {
                start: Some(service::BlockId { height: 0, hash: Vec::new() }),
                end: Some(service::BlockId { height: 0, hash: Vec::new() }),
                pool_types: Vec::new(),
            });
            let filter = service::TransparentAddressBlockFilter { address: address.clone(), range };
            // TODO: REMOVE the `GetTaddressTxids` row with the deprecated alias
            let every_method = [
                (Method::GetTaddressBalance, address_list.encode_to_vec()),
                (Method::GetTaddressBalanceStream, one_address.encode_to_vec()),
                (Method::GetAddressUtxos, utxos_arg.encode_to_vec()),
                (Method::GetAddressUtxosStream, utxos_arg.encode_to_vec()),
                (Method::GetTaddressTransactions, filter.encode_to_vec()),
                (Method::GetTaddressTxids, filter.encode_to_vec()),
            ];

            for (method, request) in every_method {
                let response = router.call(framed_request(method, request.into())).await;

                let (_, status) = answer(response.expect("infallible")).await;
                let named = format!("address {address}: {reason}");
                let expected = (Code::InvalidArgument, named.as_str());
                assert_eq!((status.code(), status.message()), expected, "{case}: {method:?}");
            }
        }
    }
}
