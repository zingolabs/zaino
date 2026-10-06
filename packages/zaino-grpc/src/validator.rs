//! The methods no index backs: `SendTransaction`, `GetTransaction` and `GetLightdInfo`.
//!
//! Every *derived* answer comes from an index, claimed by the [`Router`](crate::Router). What is
//! left is a write, a point lookup of a primary object, and one question about the server
//! itself — so this is bound on narrow source ports rather than on an aggregate serving trait.
//!
//! # Why `GetTransaction` forwards
//!
//! The boundary: consensus-critical data lives in the validator, key material lives in the
//! wallet, everything else is the indexer's. Raw transaction bytes are consensus-critical — the
//! validator stores them because it must — so indexing them here would be a second copy of the
//! chain, ~10x the compact store (compact drops proofs, signatures and 528 of each 580-byte
//! ciphertext), to serve a read that happens only for transactions a wallet already trial-
//! decrypted. Store everything, read almost none of it, and never evict.
//!
//! That is the opposite of the index rule's case. `GetAddressUtxos` and friends are *derived*:
//! forwarding them would mean a second implementation that can disagree, or a dependency on a
//! validator index (`getaddressutxos`) Zebra need not have. This one computes nothing.
//!
//! `block_height` is what Zaino can actually serve, not what the validator has. A wallet gates
//! its sync on this, so reporting the validator's tip while the index is still catching up
//! would have it request blocks that are not there yet. `estimated_height` is the validator's
//! estimate of the network tip, which is exactly the "how far behind am I" signal.

use std::{future::Future, pin::Pin, sync::Arc};

use tonic::Status;
use zaino_chainview::ChainViewSubscriber;
use zaino_index_compact_block::CompactBlockService;
use zaino_primitives::types::{
    BlockchainInfo, NetworkUpgradeStatus, TransactionId, TransactionLocation,
};
use zaino_proto::proto::service::{LightdInfo, RawTransaction, SendResponse};
use zaino_source::{GetTransaction, GetTransactionError, QueryError, SendRawTransaction};
use zcash_protocol::consensus::NetworkType;

/// Lowercase hex, so a domain id can ride out on a wire string field without a hex dependency.
fn to_hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetches one transaction's consensus bytes by txid.
///
/// `GetTaddressTransactions` is the one method needing **both** sides of the index/validator
/// boundary at once: the transparent index says *which* transactions touched an address, and the
/// validator holds their bytes. Object-safe so the router can hold one without knowing the
/// source type.
pub trait FetchRawTransaction: Send + Sync + 'static {
    /// The transaction, or a status explaining why not.
    fn fetch(
        &self,
        txid: TransactionId,
    ) -> Pin<Box<dyn Future<Output = Result<RawTransaction, Status>> + Send + '_>>;
}

impl<S: ValidatorPorts> FetchRawTransaction for ValidatorHandler<S> {
    fn fetch(
        &self,
        txid: TransactionId,
    ) -> Pin<Box<dyn Future<Output = Result<RawTransaction, Status>> + Send + '_>> {
        Box::pin(self.transaction(txid))
    }
}

/// Relays a transaction to every validator in the view.
///
/// Object-safe so the router can hold one without naming `ChainView`'s source type. Fanning out
/// is the point: N entry points propagate faster than one, one dead node cannot block a send,
/// and it is the moment the view learns the transaction is *ours*.
pub trait Relay: Send + Sync + 'static {
    /// The txid on acceptance by any endpoint.
    fn relay(
        &self,
        raw: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<TransactionId, zaino_chainview::BroadcastError>> + Send + '_,
        >,
    >;
}

impl<S: zaino_chainview::EndpointSource> Relay for zaino_chainview::ChainView<S> {
    fn relay(
        &self,
        raw: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<TransactionId, zaino_chainview::BroadcastError>> + Send + '_,
        >,
    > {
        Box::pin(self.broadcast(raw))
    }
}

/// Renders raw consensus bytes as one compact transaction.
///
/// A port, not a call: parsing consensus bytes needs a consensus library, and the serving layer
/// must not depend on a particular validator adapter to get one. The daemon, which already has
/// both, supplies the implementation.
pub trait ProjectCompact: Send + Sync + 'static {
    /// `index` is the transaction's slot in the response, which the bytes do not carry;
    /// `fee` = what a validator listed it at (`None` = not yet priced)
    fn project(
        &self,
        index: u64,
        raw: &[u8],
        fee: Option<zaino_primitives::types::Zatoshis>,
    ) -> Result<zaino_proto::proto::compact_formats::CompactTx, Status>;
}

/// What a validator must answer for the methods no index backs.
pub trait ValidatorPorts: SendRawTransaction + GetTransaction + Send + Sync + 'static {}

impl<T> ValidatorPorts for T where T: SendRawTransaction + GetTransaction + Send + Sync + 'static {}

/// Serves `SendTransaction`, `GetTransaction` & `GetLightdInfo`
///
/// - `network` = declared, never read off the validator (zebra on regtest reports `"test"`)
/// - `view` = the validators' chain description (`GetLightdInfo` never calls one)
pub struct ValidatorHandler<S> {
    source: Arc<S>,
    /// What `GetLatestBlock` serves (`LightdInfo.blockHeight` must agree with it)
    served: CompactBlockService,
    view: ChainViewSubscriber,
    network: NetworkType,
}

/// Hand-written because the source is held behind an `Arc`: deriving would demand `S: Clone`,
/// which a validator adapter owning connections deliberately is not.
impl<S> Clone for ValidatorHandler<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            served: self.served.clone(),
            view: self.view.clone(),
            network: self.network,
        }
    }
}

impl<S: ValidatorPorts> ValidatorHandler<S> {
    pub fn new(
        source: Arc<S>,
        served: CompactBlockService,
        view: ChainViewSubscriber,
        network: NetworkType,
    ) -> Self {
        Self { source, served, view, network }
    }

    /// Relays raw bytes.
    ///
    /// A rejection is a domain answer, so it rides out in the `SendResponse` with a non-zero
    /// `error_code` rather than as a transport error — a wallet needs to tell "the node said
    /// no" apart from "the node is unreachable".
    pub async fn send_transaction(&self, raw: RawTransaction) -> SendResponse {
        match self.source.send_raw_transaction(raw.data.to_vec()).await {
            Ok(txid) => SendResponse { error_code: 0, error_message: to_hex(txid.into()) },
            Err(rejection) => SendResponse { error_code: -1, error_message: rejection.to_string() },
        }
    }

    /// Fetches one transaction's consensus bytes by txid.
    ///
    /// `height` is the mined height, or `0` for an unmined one — the wire's own convention for
    /// "in the mempool", which is also what a `TransactionLocation` carrying no height means.
    pub async fn transaction(&self, txid: TransactionId) -> Result<RawTransaction, Status> {
        let found = self.source.get_transaction(txid).await.map_err(|error| match error {
            QueryError::Domain(GetTransactionError::NotFound(txid)) => {
                Status::not_found(format!("transaction not found: {txid}"))
            }
            other => Status::unavailable(other.to_string()),
        })?;

        // Orphaned reads as unmined: the wire has one "no height" value, and a client must not
        // treat a branch the chain abandoned as confirmed.
        let height = match found.location {
            TransactionLocation::BestChain(at) => at.into(),
            TransactionLocation::NonBestChain | TransactionLocation::Mempool => 0,
        };

        Ok(RawTransaction { data: found.bytes.into(), height })
    }

    /// Serving metadata, the served height, and the validators' view of the network
    ///
    /// - one pinned view, no validator call (validator half = as of its last poll tick)
    /// - below quorum = `UNAVAILABLE` (no stand-in branch, schedule or tip)
    /// - TODO: populate `lightwalletProtocolVersion`, pending ZIP updates to the light client
    ///   protocol (unset pins spec-following clients to the shielded-only `GetBlockRange` default)
    pub fn lightd_info(&self) -> Result<LightdInfo, Status> {
        let pinned = self.view.current();
        let chain =
            pinned.validator_info().map_err(|below| Status::unavailable(below.to_string()))?;
        // empty index: 0 (the proto has no "none")
        let block_height = self.served.tip().map_or(0, u64::from);

        Ok(with_validator_view(
            LightdInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                vendor: "zaino".to_string(),
                taddr_support: true,
                chain_name: chain_name(self.network).to_string(),
                block_height,
                ..Default::default()
            },
            chain,
        ))
    }
}

/// lightwalletd's names (BIP70), not zaino's config spelling
fn chain_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    }
}

/// Upgrade schedule + branch = validator's (consensus data, see `docs/design/boundaries.md`)
fn with_validator_view(info: LightdInfo, chain: &BlockchainInfo) -> LightdInfo {
    let next_pending = chain
        .upgrades
        .iter()
        .filter(|upgrade| upgrade.status == NetworkUpgradeStatus::Pending)
        .min_by_key(|upgrade| upgrade.activation_height);

    LightdInfo {
        sapling_activation_height: chain.sapling_activation.into(),
        consensus_branch_id: chain.consensus.chain_tip.to_string(),
        estimated_height: chain.estimated_height.into(),
        upgrade_name: next_pending.map_or_else(String::new, |upgrade| upgrade.name.clone()),
        upgrade_height: next_pending.map_or(0, |upgrade| upgrade.activation_height.into()),
        ..info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        ConsensusBranchId, ConsensusBranchIds, Height, NetworkUpgradeInfo,
    };

    /// Next pending = lowest pending height (validator order is not a schedule); none → empty/0
    #[test]
    fn lightd_info_takes_the_schedule_and_branch_from_the_validator_and_the_chain_from_config() {
        let at = |h: u32| Height::try_from(h).expect("in range");
        let upgrade = |name: &str, branch: u32, h: u32, status| NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: name.to_owned(),
            activation_height: at(h),
            status,
        };
        let schedule = vec![
            upgrade("Overwinter", 0x5ba8_1b19, 347_500, NetworkUpgradeStatus::Active),
            upgrade("Sapling", 0x76b8_09bb, 419_200, NetworkUpgradeStatus::Active),
            upgrade("NU5", 0xc2d6_d0b4, 1_687_104, NetworkUpgradeStatus::Active),
            upgrade("NU7", 0x7777_7777, 4_000_000, NetworkUpgradeStatus::Pending),
            upgrade("NU6.3", 0x6363_6363, 3_428_143, NetworkUpgradeStatus::Pending),
        ];
        let nu5 = ConsensusBranchId::new(0xc2d6_d0b4);
        let chain = BlockchainInfo {
            blocks: at(3_426_990),
            estimated_height: at(3_427_000),
            best_block_hash: [7u8; 32].into(),
            sapling_activation: at(419_200),
            upgrades: schedule,
            consensus: ConsensusBranchIds { chain_tip: nu5, next_block: nu5 },
        };
        let served = LightdInfo {
            vendor: "zaino".to_owned(),
            chain_name: chain_name(NetworkType::Main).to_owned(),
            block_height: 3_400_000,
            estimated_height: 3_400_000,
            ..Default::default()
        };

        let expected = LightdInfo {
            sapling_activation_height: 419_200,
            consensus_branch_id: "c2d6d0b4".to_owned(),
            estimated_height: 3_427_000,
            upgrade_name: "NU6.3".to_owned(),
            upgrade_height: 3_428_143,
            ..served.clone()
        };
        assert_eq!(with_validator_view(served.clone(), &chain), expected);

        let settled = BlockchainInfo {
            estimated_height: at(4_100_000),
            upgrades: chain
                .upgrades
                .iter()
                .cloned()
                .map(|u| NetworkUpgradeInfo { status: NetworkUpgradeStatus::Active, ..u })
                .collect(),
            ..chain
        };
        let settled = with_validator_view(served, &settled);
        let upgrade = (settled.upgrade_name.as_str(), settled.upgrade_height);
        assert_eq!(upgrade, ("", 0), "none scheduled");

        let networks = [NetworkType::Main, NetworkType::Test, NetworkType::Regtest];
        assert_eq!(networks.map(chain_name), ["main", "test", "regtest"]);
    }

    /// Before the view's first poll: UNAVAILABLE naming the shortfall (no stand-in tip); after
    /// it: the poller's `getblockchaininfo` + the served height (sync fn = no validator call)
    #[tokio::test]
    async fn lightd_info_refuses_below_quorum_then_answers_from_the_polled_view() {
        let validator = Arc::new((0..=7).fold(zaino_source::mock::MockChain::new(), |chain, h| {
            chain.with_block(zaino_source::mock::test_block(h, h as u8 + 1))
        }));
        let depth = zaino_primitives::types::ReorgDepth::new(
            std::num::NonZeroU32::new(3).expect("non-zero"),
        );
        let endpoint = zaino_chainview::Endpoint {
            address: "one:8232".to_owned(),
            source: Arc::clone(&validator),
        };
        let (view, pollers) = zaino_chainview::ChainView::new(vec![endpoint], depth).expect("one");
        let served = CompactBlockService::new(zaino_sync::Served::fixed(
            zaino_index_compact_block::CompactBlockStore::open(
                zaino_persistence::fs::SimFs::new(),
                std::path::Path::new("/cb"),
                NetworkType::Main,
            )
            .expect("open")
            .reader()
            .pin(),
        ));
        let handler =
            ValidatorHandler::new(validator, served, view.subscriber(), NetworkType::Main);

        let refused = handler.lightd_info().expect_err("nothing polled yet");
        let shortfall = (refused.code(), refused.message());
        let expected = "0 of 1 validators agree on a tip; 1 required";
        assert_eq!(shortfall, (tonic::Code::Unavailable, expected));

        let cancel = tokio_util::sync::CancellationToken::new();
        let polling = pollers.into_iter().map(|poller| tokio::spawn(poller.run(cancel.clone())));
        let polling: Vec<_> = polling.collect();
        let mut tip = view.subscriber().subscribe_tip();
        tip.wait_for(Option::is_some).await.expect("view alive");

        let expected = LightdInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            vendor: "zaino".to_owned(),
            taddr_support: true,
            chain_name: "main".to_owned(),
            consensus_branch_id: "00000000".to_owned(),
            estimated_height: 7,
            block_height: 0,
            ..Default::default()
        };
        assert_eq!(handler.lightd_info().expect("quorum met"), expected, "mock: tip 7, no index");

        cancel.cancel();
        for poller in polling {
            poller.await.expect("poller ran to its cancel");
        }
    }
}
