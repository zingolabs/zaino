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

pub(crate) fn get_lightd_info<V>(
    grpc_snapshot: &GrpcSnapshot<V>,
    network: NetworkType,
    version: &str,
) -> Result<Response<Body>, Status> {
    let lightd_info = lightd_info(grpc_snapshot, network, version)?;
    Ok(unary_response(frame(&lightd_info)))
}

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
    use zaino_header_chain::HeaderChainSnapshot;
    use zaino_persistence::DiskView;
    use zaino_primitives::protocol::compiled_schedule;
    use zaino_source::testing::{h, MockChain};
    use zcash_protocol::consensus::BlockHeight;

    use crate::testing::{fixed_snapshots, indexed_at, published, routes_over, until, MAINNET};

    /// `consensus_branch_id` as the wire spells it
    fn branch(id: BranchId) -> String {
        format!("{:08x}", u32::from(id))
    }

    /// Mainnet's compiled schedule at the served height: NU6.2 from 3,364,600, NU6.3 from
    /// 3,428,143, NU7 unscheduled
    #[test]
    fn lightd_info_takes_branch_and_next_upgrade_from_the_schedule_at_the_served_height() {
        let at = BlockHeight::from;
        let schedule = compiled_schedule(NetworkType::Main).expect("compiled");
        let base = LightdInfo {
            version: "0.11.0-alpha.2".to_owned(),
            vendor: "zaino".to_owned(),
            taddr_support: true,
            chain_name: "main".to_owned(),
            sapling_activation_height: 419_200,
            estimated_height: 3_500_000,
            lightwallet_protocol_version: zaino_proto::LIGHTWALLET_PROTOCOL_VERSION.to_owned(),
            ..Default::default()
        };
        #[rustfmt::skip]
        let cases = [
            ("NU6.2 in force, NU6.3 next", 3_400_000, LightdInfo {
                consensus_branch_id: branch(BranchId::Nu6_2),
                block_height: 3_400_000,
                upgrade_name: "Nu6.3".to_owned(),
                upgrade_height: 3_428_143,
                ..base.clone()
            }),
            ("NU6.3 in force, nothing next", 3_428_143, LightdInfo {
                consensus_branch_id: branch(BranchId::Nu6_3),
                block_height: 3_428_143,
                ..base.clone()
            }),
        ];
        for (case, served, expected) in cases {
            let lightd = Lightd { served: at(served), best: at(3_500_000), schedule };

            let info = lightd_info_message(lightd, NetworkType::Main, "0.11.0-alpha.2");

            assert_eq!(info, expected, "{case}: served {served}, best 3,500,000");
        }
    }

    /// Validator never polled (no holder, ever); the chain's schedule = every upgrade through NU6.3
    /// from 1
    #[tokio::test(start_paused = true)]
    async fn lightd_info_answers_once_served_with_no_validator_holding_the_tip() {
        use zaino_source::testing::within;

        let mut chain = MockChain::regtest();
        let tip_7 = chain.mine_empty(7);
        let node = chain.validator(tip_7);
        let (mut routes, _never_driven, _never_merged) = routes_over(&node, MAINNET, &[]);
        let (publisher, nfs) = published(&mut routes);
        let grpc_snapshots = routes.grpc_snapshots.clone();
        let info = || lightd_info(&grpc_snapshots.load(), NetworkType::Main, routes.version);
        let refusal = || info().err().map(|status| (status.code(), status.message().to_owned()));
        let cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(publisher.run(cancel.clone()));
        let unserved = Some((
            tonic::Code::Unavailable,
            "the indexes are syncing: nothing served yet".to_owned(),
        ));

        assert_eq!(refusal(), unserved, "no chain yet");
        routes.chain_view.set_chain(Some(HeaderChainSnapshot::of(&chain, tip_7)));
        within("best 7 published", until(|| grpc_snapshots.load().tips().best == Some(tip_7)))
            .await;
        assert_eq!(refusal(), unserved, "verified 7, nothing served");

        nfs.send_replace(Some(Arc::new(indexed_at(&chain, chain.at(h(5)), Vec::new()))));
        within("5 served", until(|| info().is_ok())).await;

        let expected = LightdInfo {
            version: "0.11.0-test".to_owned(),
            vendor: "zaino".to_owned(),
            taddr_support: true,
            chain_name: "main".to_owned(),
            sapling_activation_height: 1,
            consensus_branch_id: branch(BranchId::Nu6_3),
            estimated_height: 7,
            block_height: 5,
            lightwallet_protocol_version: zaino_proto::LIGHTWALLET_PROTOCOL_VERSION.to_owned(),
            ..Default::default()
        };
        assert_eq!(info().ok(), Some(expected), "best 7, NU6.3's branch at 5, 5 served");
        cancel.cancel();
    }

    /// Rule at best + 1: mainnet's legacy 5,000 this low, regtest's 1,000; no chain = refused
    #[test]
    fn standard_fee_is_the_rule_at_the_block_after_best_with_no_validator_read() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let held =
            fixed_snapshots(&chain, tip, Vec::new(), TrustedValidatorSet::default(), &[]).load();
        let empty = ChainViewSnapshot::fixed(None, TrustedValidatorSet::default(), &[], &[]);
        let unanchored = GrpcSnapshots::<DiskView>::fixed(None, Arc::new(empty)).load();
        let fee = |grpc_snapshot, network| {
            standard_fee(grpc_snapshot, network).map(|fee| fee.fee_zat).map_err(|s| s.code())
        };

        assert_eq!(fee(&held, NetworkType::Main), Ok(5_000), "mainnet, best 5");
        assert_eq!(fee(&held, NetworkType::Regtest), Ok(1_000), "regtest, best 5");
        let refused = fee(&unanchored, NetworkType::Main);
        assert_eq!(refused, Err(tonic::Code::Unavailable), "no header chain");
    }
}
