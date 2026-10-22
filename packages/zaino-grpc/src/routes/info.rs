//! Server-info methods: `GetLightdInfo`, `GetStandardFee`, both from the `GrpcSnapshot`'s tips +
//! schedule, no validator read
//!
//! - `block_height` = what Zaino serves, not the validator's tip (a wallet gates its sync on it)
//! - `estimated_height` = verified best tip, no validator call ("how far behind")

use http::Response;
use tonic::{body::Body, Status};
use zaino_grpc_snapshot::{GrpcSnapshot, Lightd, Unavailable};
use zaino_proto::proto::service::{self, LightdInfo};
use zcash_protocol::consensus::{
    BranchId, NetworkType, NetworkUpgrade, Parameters, UPGRADES_IN_ORDER,
};

use crate::wire::{self, frame, unary_response};

/// `GetLightdInfo`
pub(crate) fn get_lightd_info<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    network: NetworkType,
    version: &str,
) -> Result<Response<Body>, Status> {
    let lightd_info = lightd_info(grpc_snapshot, network, version)?;
    Ok(unary_response(frame(&lightd_info)))
}

/// `GetStandardFee`
pub(crate) fn get_standard_fee<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    network: NetworkType,
) -> Result<Response<Body>, Status> {
    let standard_fee = standard_fee(grpc_snapshot, network)?;
    Ok(unary_response(frame(&standard_fee)))
}

/// `GetLightdInfo`'s message from one `GrpcSnapshot` (fields: `docs/rpc_api.md`)
fn lightd_info<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    network: NetworkType,
    version: &str,
) -> Result<LightdInfo, Status> {
    let lightd = grpc_snapshot.lightd().map_err(wire::unavailable)?;
    Ok(lightd_info_message(lightd, network, version))
}

/// Served tips + schedule → wire `LightdInfo`
fn lightd_info_message(lightd: Lightd, network: NetworkType, version: &str) -> LightdInfo {
    let schedule = lightd.schedule;
    let next = UPGRADES_IN_ORDER
        .iter()
        .filter_map(|&upgrade| schedule.activation_height(upgrade).map(|from| (upgrade, from)))
        .filter(|(_, from)| *from > lightd.served)
        .min_by_key(|(_, from)| *from);
    let sapling = schedule.activation_height(NetworkUpgrade::Sapling);
    LightdInfo {
        version: version.to_string(),
        vendor: "zaino".to_string(),
        taddr_support: true,
        chain_name: chain_name(network).to_string(),
        sapling_activation_height: sapling.map_or(0, u64::from),
        consensus_branch_id: format!(
            "{:08x}",
            u32::from(BranchId::for_height(&schedule, lightd.served))
        ),
        block_height: u64::from(lightd.served),
        estimated_height: u64::from(lightd.best),
        upgrade_name: next.map_or_else(String::new, |(upgrade, _)| upgrade.to_string()),
        upgrade_height: next.map_or(0, |(_, at)| u64::from(at)),
        lightwallet_protocol_version: zaino_proto::LIGHTWALLET_PROTOCOL_VERSION.to_string(),
        ..Default::default()
    }
}

/// lightwalletd `chainName` / `z_gettreestate` `network` (BIP70, `regtest` = de-facto third)
///
/// - lightwalletd's `NetworkType` spelling (zainod's config spelling = `network_name` there)
pub(super) fn chain_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    }
}

/// `GetStandardFee`'s message: the ZIP 317 marginal fee for the block after best, no validator read
/// (estimator version 0 = the static rule)
fn standard_fee<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    network: NetworkType,
) -> Result<service::StandardFee, Status> {
    let best = grpc_snapshot.tips().best.ok_or(Unavailable::NoChain).map_err(wire::unavailable)?;
    let fee = zaino_primitives::protocol::standard_fee(network, best.height + 1);
    Ok(service::StandardFee { fee_zat: u64::from(fee), estimator_version: 0 })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use zaino_chainview::{ChainViewSnapshot, TrustedValidatorSet};
    use zaino_grpc_snapshot::GrpcSnapshots;
    use zaino_header_chain::testing::HeaderViews;
    use zaino_persistence::DiskView;
    use zaino_primitives::protocol::compiled_schedule;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_source::testing::ValidatorViews;
    use zcash_protocol::consensus::BlockHeight;

    use crate::testing::{grpc_snapshots, indexed_at, published, rounds, routes_over};

    #[test]
    fn each_network_has_one_chain_name() {
        let table = [
            (NetworkType::Main, "main"),
            (NetworkType::Test, "test"),
            (NetworkType::Regtest, "regtest"),
        ];
        for (network, chain) in table {
            assert_eq!(chain_name(network), chain, "{network:?}");
        }
    }

    /// Mainnet's compiled schedule: branch at the served height (NU6.2 from 3,364,600 below
    /// NU6.3, NU6.3 from it), the next upgrade above it (none once NU6.3 is in: NU7
    /// unscheduled), the best as the estimate, the chain + version from the daemon
    #[test]
    fn lightd_info_takes_branch_and_next_upgrade_from_the_schedule_at_the_served_height() {
        let at = BlockHeight::from;
        let schedule = compiled_schedule(NetworkType::Main).expect("compiled");
        let lightd = |served: u32| Lightd { served: at(served), best: at(3_500_000), schedule };
        let rendered = |served: u32| {
            let info = lightd_info_message(lightd(served), NetworkType::Main, "0.11.0-alpha.2");
            (
                info.version,
                info.chain_name,
                info.sapling_activation_height,
                info.consensus_branch_id,
                (info.block_height, info.estimated_height),
                (info.upgrade_name, info.upgrade_height),
            )
        };
        let (version, main) = (|| "0.11.0-alpha.2".to_owned(), || "main".to_owned());
        assert_eq!(
            rendered(3_400_000),
            (
                version(),
                main(),
                419_200,
                "5437f330".to_owned(),
                (3_400_000, 3_500_000),
                ("Nu6.3".to_owned(), 3_428_143)
            )
        );
        assert_eq!(
            rendered(3_428_143),
            (
                version(),
                main(),
                419_200,
                "37a5165b".to_owned(),
                (3_428_143, 3_500_000),
                (String::new(), 0)
            ),
            "NU6.3 in force, nothing scheduled after"
        );
    }

    /// Over a live publisher, its validator never polled (no holder, ever):
    /// - `UNAVAILABLE` until something is served (no chain yet, then verified but unserved)
    /// - served at 5 under the chain's schedule (every upgrade through NU6.3 from 1): NU6.3's
    ///   branch, the verified best 7 as the estimate, 5 served (one load, no validator)
    #[tokio::test(start_paused = true)]
    async fn lightd_info_answers_once_served_with_no_validator_holding_the_tip() {
        let mut chain = MockChain::regtest();
        let tip_7 = chain.mine_empty(7);
        let node = Arc::new(chain.validator(tip_7));
        let (mut routes, _balancing, _fold) = routes_over(&node);
        let (publisher, nfs) = published(&mut routes);
        let grpc_snapshots = routes.grpc_snapshots.clone();
        let info = || lightd_info(&grpc_snapshots.load(), NetworkType::Main, routes.version);
        let refused = |why: &str| {
            let refused = info().map_err(|status| (status.code(), status.message().to_owned()));
            refused.err() == Some((tonic::Code::Unavailable, why.to_owned()))
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.clone()));

        let unserved = "the indexes are syncing: nothing served yet";
        assert!(refused(unserved), "no chain yet");
        routes.chain_view.set_chain(Some(chain.snapshot(tip_7)));
        rounds("verified, nothing served", || refused(unserved)).await;

        nfs.send_replace(Some(Arc::new(indexed_at(&chain, chain.at(h(5)), Vec::new()))));
        rounds("served", || info().is_ok()).await;
        let expected = LightdInfo {
            version: "0.11.0-test".to_owned(),
            vendor: "zaino".to_owned(),
            taddr_support: true,
            chain_name: "main".to_owned(),
            sapling_activation_height: 1,
            consensus_branch_id: "37a5165b".to_owned(),
            estimated_height: 7,
            block_height: 5,
            lightwallet_protocol_version: "v0.6.0".to_owned(),
            ..Default::default()
        };
        assert_eq!(info().expect("served"), expected, "best 7, NU6.3's branch at 5, 5 served");
        let held_by = grpc_snapshots.load().chain_view().held_by();
        assert!(held_by.is_empty(), "no validator ever held the tip");
        cancel.cancel();
    }

    /// `GetStandardFee` from the snapshot alone: the rule at best + 1 (mainnet's legacy 5,000 this
    /// low, regtest's 1,000), `UNAVAILABLE` with no header chain
    #[test]
    fn standard_fee_is_the_rule_at_the_block_after_best_with_no_validator_read() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let held = grpc_snapshots(&chain, tip, Vec::new()).load();
        let fee = |network| standard_fee(&held, network).map(|fee| fee.fee_zat).ok();
        assert_eq!((fee(NetworkType::Main), fee(NetworkType::Regtest)), (Some(5_000), Some(1_000)));
        let empty = ChainViewSnapshot::fixed(None, TrustedValidatorSet::default(), &[], &[]);
        let unanchored = GrpcSnapshots::<DiskView>::fixed(None, Arc::new(empty)).load();
        let refused = standard_fee(&unanchored, NetworkType::Main).map_err(|status| status.code());
        assert_eq!(refused, Err(tonic::Code::Unavailable), "no header chain yet");
    }
}
