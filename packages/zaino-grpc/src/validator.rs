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

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use tokio::time::Instant;
use tonic::Status;
use zaino_index_compact_block::CompactBlockService;
use zaino_primitives::types::{
    BlockchainInfo, NetworkUpgradeStatus, TransactionId, TransactionLocation,
};
use zaino_proto::proto::service::{LightdInfo, RawTransaction, SendResponse};
use zaino_source::{
    GetBlockchainInfo, GetTransaction, GetTransactionError, QueryError, SendRawTransaction,
};
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
pub trait ValidatorPorts:
    SendRawTransaction + GetTransaction + GetBlockchainInfo + Send + Sync + 'static
{
}

impl<T> ValidatorPorts for T where
    T: SendRawTransaction + GetTransaction + GetBlockchainInfo + Send + Sync + 'static
{
}

/// How long one `getblockchaininfo` answers `GetLightdInfo` (wallets poll it every few seconds;
/// its fields move once a block at most)
const CHAIN_INFO_TTL: Duration = Duration::from_secs(1);

/// Serves `SendTransaction`, `GetTransaction` & `GetLightdInfo` over one validator
///
/// `network` = declared, never read off the validator (zebra on regtest reports `"test"`)
pub struct ValidatorHandler<S> {
    source: Arc<S>,
    /// What `GetLatestBlock` serves (`LightdInfo.blockHeight` must agree with it)
    served: CompactBlockService,
    network: NetworkType,
    /// Last `getblockchaininfo` + when (shared by clones; a failure is never kept)
    chain: Arc<tokio::sync::Mutex<Option<(Instant, BlockchainInfo)>>>,
}

/// Hand-written because the source is held behind an `Arc`: deriving would demand `S: Clone`,
/// which a validator adapter owning connections deliberately is not.
impl<S> Clone for ValidatorHandler<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            served: self.served.clone(),
            network: self.network,
            chain: Arc::clone(&self.chain),
        }
    }
}

impl<S: ValidatorPorts> ValidatorHandler<S> {
    pub fn new(source: Arc<S>, served: CompactBlockService, network: NetworkType) -> Self {
        Self { source, served, network, chain: Arc::default() }
    }

    /// The validator's chain view, at most [`CHAIN_INFO_TTL`] old
    ///
    /// - refresh under the lock = one RPC in flight however many wallets ask at once
    async fn chain_info(&self) -> Result<BlockchainInfo, Status> {
        let mut cached = self.chain.lock().await;
        if let Some((at, info)) = cached.as_ref() {
            if at.elapsed() < CHAIN_INFO_TTL {
                return Ok(info.clone());
            }
        }

        let info = self
            .source
            .get_blockchain_info()
            .await
            .map_err(|error| Status::unavailable(format!("validator: {error}")))?;
        *cached = Some((Instant::now(), info.clone()));
        Ok(info)
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

    /// Serving metadata, the served height, and the validator's view of the network
    ///
    /// - validator unreachable = `UNAVAILABLE` (no stand-in branch, schedule or tip)
    /// - validator half ≤ [`CHAIN_INFO_TTL`] old; `block_height` always current
    /// - TODO: populate `lightwalletProtocolVersion`, pending ZIP updates to the light client
    ///   protocol (unset pins spec-following clients to the shielded-only `GetBlockRange` default)
    pub async fn lightd_info(&self) -> Result<LightdInfo, Status> {
        let chain = self.chain_info().await?;
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
            &chain,
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

    /// Counts `getblockchaininfo` calls; `failing` = answers unreachable
    #[derive(Default)]
    struct CountingValidator {
        calls: std::sync::atomic::AtomicUsize,
        failing: std::sync::atomic::AtomicBool,
    }

    impl GetBlockchainInfo for CountingValidator {
        async fn get_blockchain_info(
            &self,
        ) -> Result<BlockchainInfo, QueryError<zaino_source::GetBlockchainInfoError>> {
            use std::sync::atomic::Ordering::SeqCst;
            let call = self.calls.fetch_add(1, SeqCst) + 1;
            tokio::task::yield_now().await;
            if self.failing.load(SeqCst) {
                return Err(QueryError::NonDomain(zaino_source::NonDomainError::new(
                    zaino_source::FailureMode::Connection,
                    "validator down",
                )));
            }
            let at = Height::try_from(1_000 + call as u32).expect("in range");
            let branch = ConsensusBranchId::new(0xc2d6_d0b4);
            Ok(BlockchainInfo {
                blocks: at,
                estimated_height: at,
                best_block_hash: [7u8; 32].into(),
                sapling_activation: Height::try_from(1).expect("in range"),
                upgrades: Vec::new(),
                consensus: ConsensusBranchIds { chain_tip: branch, next_block: branch },
            })
        }
    }

    impl SendRawTransaction for CountingValidator {
        async fn send_raw_transaction(
            &self,
            _: Vec<u8>,
        ) -> Result<TransactionId, QueryError<zaino_source::SendRawTransactionError>> {
            unreachable!("LightdInfo relays nothing")
        }
    }

    impl GetTransaction for CountingValidator {
        async fn get_transaction(
            &self,
            _: TransactionId,
        ) -> Result<zaino_source::TransactionResponse, QueryError<GetTransactionError>> {
            unreachable!("LightdInfo reads no transaction")
        }
    }

    /// A poll storm costs one validator RPC per TTL: concurrent askers share the one in flight,
    /// an expired answer refreshes, and a failure is answered UNAVAILABLE but never kept
    #[tokio::test(start_paused = true)]
    async fn lightd_info_asks_the_validator_at_most_once_per_ttl_and_never_caches_a_failure() {
        use std::sync::atomic::Ordering::SeqCst;

        let validator = Arc::new(CountingValidator::default());
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
        let handler = ValidatorHandler::new(Arc::clone(&validator), served, NetworkType::Main);
        let estimated =
            |info: Result<LightdInfo, Status>| info.ok().map(|info| info.estimated_height);

        let storm = futures::future::join_all((0..8).map(|_| handler.clone().lightd_info_owned()));
        let answers: Vec<_> = storm.await.into_iter().map(estimated).collect();
        assert_eq!(answers, [Some(1_001); 8]);
        assert_eq!(validator.calls.load(SeqCst), 1, "eight concurrent askers, one RPC");

        tokio::time::advance(CHAIN_INFO_TTL - Duration::from_millis(1)).await;
        assert_eq!(estimated(handler.lightd_info().await), Some(1_001), "still fresh");
        assert_eq!(validator.calls.load(SeqCst), 1);

        tokio::time::advance(Duration::from_millis(1)).await;
        validator.failing.store(true, SeqCst);
        let down = handler.lightd_info().await.expect_err("validator down");
        assert_eq!(down.code(), tonic::Code::Unavailable, "{down:?}");
        validator.failing.store(false, SeqCst);
        let recovered = estimated(handler.lightd_info().await);
        assert_eq!(recovered, Some(1_003), "the failure was not kept");
        assert_eq!(validator.calls.load(SeqCst), 3);
    }

    impl<S: ValidatorPorts> ValidatorHandler<S> {
        async fn lightd_info_owned(self) -> Result<LightdInfo, Status> {
            self.lightd_info().await
        }
    }
}
