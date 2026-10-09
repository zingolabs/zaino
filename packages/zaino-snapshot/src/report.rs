//! Reporting from one snapshot (G9): `/statusz` ([`Report`]), gauges at scrape ([`emit_gauges`]),
//! edge logs per publish ([`transitions`])
//!
//! - State = f(snapshot, `SyncProgress`, `MemberTable`): nothing mirrored between scrapes
//! - Gauge names = ztest's `zainod` families (a rename breaks its sync probes)

use serde::Serialize;
use tracing::{info, warn};
use zaino_chainview::{Agreement, ChainViewSnapshot, EndpointSet, Spread};
use zaino_nfs::INDEXES;
use zaino_persistence::{IndexKind, View};
use zaino_primitives::types::{self, BlockRef};
use zaino_sync::SyncProgress;
use zaino_traffic::{Health, MemberId, MemberTable, ValidatorId};

use crate::snapshot::Snapshot;

mod names {
    pub(super) const BEST_TIP: &str = "zaino.best_tip";
    pub(super) const FETCH_HEIGHT: &str = "zaino.fetch_height";
    pub(super) const INDEX_SYNCED: &str = "zaino.index.synced";
    pub(super) const ENDPOINT_STATE: &str = "zaino.chainview.endpoint_state";
    pub(super) const AGREEMENT: &str = "zaino.chainview.agreement";
    pub(super) const TIP_HEIGHT: &str = "zaino.chainview.tip_height";
    pub(super) const STALE_BLOCKS: &str = "zaino.chainview.stale_blocks";
    pub(super) const PEERS: &str = "zaino.chainview.peers";
    pub(super) const PUSH_STREAM: &str = "zaino.chainview.push_stream";
    pub(super) const RELEASE: &str = "zaino.chainview.release";
    pub(super) const END_OF_SERVICE_HEIGHT: &str = "zaino.chainview.end_of_service_height";
    pub(super) const TIP_HOLDERS: &str = "zaino.chainview.tip_holders";
    pub(super) const BEST_HEIGHT: &str = "zaino.chainview.best_height";
    pub(super) const FINALITY_PAUSED: &str = "zaino.chainview.finality_paused";
    pub(super) const SHARED_OUTBOUND_MIN: &str = "zaino.chainview.shared_outbound_min";
    pub(super) const MEMPOOL_TRANSACTIONS: &str = "zaino.chainview.mempool_transactions";
}

const STATES: [Health; 5] =
    [Health::Pending, Health::Live, Health::Degraded, Health::Down, Health::CatchingUp];

const AGREEMENTS: [Agreement; 5] = [
    Agreement::Unknown,
    Agreement::Agreed,
    Agreement::Ahead,
    Agreement::Behind,
    Agreement::Diverged,
];

/// `/statusz` body (zainod adds its process fields beside it)
///
/// - `handed` = last block handed to the indexes
#[derive(Debug, Serialize, PartialEq)]
pub struct Report {
    seq: u64,
    tips: TipsReport,
    unready: Vec<&'static str>,
    handed: Option<u32>,
    indexes: Vec<IndexReport>,
    validators: Vec<ValidatorReport>,
    alarms: AlarmsReport,
    mempool: MempoolCounts,
    forks: Vec<ForkReport>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Block {
    height: u32,
    hash: String,
}

#[derive(Debug, Serialize, PartialEq)]
struct TipsReport {
    best: Option<Block>,
    #[serde(rename = "final")]
    final_tip: Option<Block>,
    served: Option<Block>,
    held_by: usize,
    configured: usize,
    synced: bool,
}

/// One `INDEXES` kind: `enabled` = config's, `durable` = its committed height
#[derive(Debug, Serialize, PartialEq)]
pub struct IndexReport {
    name: &'static str,
    enabled: bool,
    durable: Option<u32>,
}

/// Every `INDEXES` kind, `durable` absent before the NFS's first publish (zainod's pre-boot stub
/// passes none)
pub fn indexes(
    enabled: &[IndexKind],
    durable: &[(IndexKind, Option<BlockRef>)],
) -> Vec<IndexReport> {
    let height = |kind| durable.iter().find(|(each, _)| *each == kind).and_then(|(_, tip)| *tip);
    let report = |kind: IndexKind| IndexReport {
        name: kind.name(),
        enabled: enabled.contains(&kind),
        durable: height(kind).map(|tip| u32::from(tip.height)),
    };
    INDEXES.into_iter().map(report).collect()
}

/// Chain view facts ⨝ the balancer's `MemberTable` row (`state`, `latency_ms`, `failures`)
///
/// - `latency_ms` `None` until its first answer; `release` `None` until a metadata read answers
#[derive(Debug, Serialize, PartialEq)]
struct ValidatorReport {
    address: String,
    state: &'static str,
    agreement: &'static str,
    height: Option<u32>,
    stale_blocks: Option<u32>,
    latency_ms: Option<u64>,
    failures: u32,
    observed_s_ago: Option<u64>,
    streaming: bool,
    release: Option<Release>,
    peers: Vec<Peer>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Release {
    build: String,
    user_agent: String,
    protocol_version: u32,
    end_of_service: EndOfService,
}

/// `blocks_left` = from the validator's own tip
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EndOfService {
    At { height: u32, estimated_unix: i64, blocks_left: Option<u32> },
    NotEnforced,
    Unknown,
}

#[derive(Debug, Serialize, PartialEq)]
struct Peer {
    addr: String,
    inbound: bool,
}

/// `stale`, `ending` = validator addresses
#[derive(Debug, Serialize, PartialEq)]
struct AlarmsReport {
    partitioned: bool,
    eclipsed: bool,
    finality_paused: bool,
    stale: Vec<String>,
    ending: Vec<String>,
}

/// `transactions` = held (servable or not); `trusted_readers` = the `y` of `trusted: x/y`
#[derive(Debug, Serialize, PartialEq)]
struct MempoolCounts {
    transactions: usize,
    verified: usize,
    ours_unverified: usize,
    fully_spread: usize,
    trusted_readers: usize,
}

/// `cumulative_work` decimal (past 2^53: a JSON number loses it)
#[derive(Debug, Serialize, PartialEq)]
struct ForkReport {
    from: Block,
    tip: Block,
    cumulative_work: String,
    folded: Option<Block>,
}

impl Report {
    /// - `members` = the traffic balancer's table (joined on `MemberId::Trusted(position)`)
    /// - `enabled` = config's indexes (listed before the NFS's first publish)
    pub fn of<V: View>(
        snap: &Snapshot<V>,
        progress: &SyncProgress,
        members: &MemberTable,
        enabled: &[IndexKind],
    ) -> Self {
        let (tips, view) = (snap.tips(), snap.view());
        let endpoints = view.endpoints();
        let named = |set: EndpointSet| -> Vec<String> {
            let at = set.positions().filter_map(|position| endpoints.get(position));
            at.map(|meta| meta.address.clone()).collect()
        };
        let durable: Vec<_> = snap.indexed().into_iter().flat_map(|i| i.durable()).collect();
        let indexes = indexes(enabled, &durable);
        let validators = endpoints
            .iter()
            .enumerate()
            .map(|(position, meta)| {
                let member = ValidatorId::new(position)
                    .and_then(|id| members.rows.iter().find(|row| row.id == MemberId::Trusted(id)));
                ValidatorReport {
                    address: meta.address.clone(),
                    state: member.map_or(Health::Pending, |row| row.health).label(),
                    agreement: meta.agreement.label(),
                    height: meta.tip().map(|tip| u32::from(tip.height)),
                    stale_blocks: meta.stale_blocks(),
                    latency_ms: member
                        .filter(|row| row.health != Health::Pending)
                        .map(|row| row.latency.as_millis() as u64),
                    failures: member.map_or(0, |row| row.failures),
                    observed_s_ago: meta.observed_at.map(|at| at.elapsed().as_secs()),
                    streaming: meta.streaming,
                    release: meta.release.as_ref().map(|release| Release {
                        build: release.build.clone(),
                        user_agent: release.user_agent.clone(),
                        protocol_version: release.protocol_version,
                        end_of_service: match release.end_of_service {
                            types::EndOfService::At { height, estimated_unix } => {
                                EndOfService::At {
                                    height: u32::from(height),
                                    estimated_unix,
                                    blocks_left: meta.blocks_to_end_of_service(),
                                }
                            }
                            types::EndOfService::NotEnforced => EndOfService::NotEnforced,
                            types::EndOfService::Unknown => EndOfService::Unknown,
                        },
                    }),
                    peers: meta
                        .peers
                        .iter()
                        .map(|peer| Peer { addr: peer.addr.clone(), inbound: peer.inbound })
                        .collect(),
                }
            })
            .collect();
        let alarms = view.alarms();
        let readers = view.mempool_readers().count();
        let spreads: Vec<_> = view.spreads().map(|(_, spread)| spread).collect();
        let count = |of: &dyn Fn(&Spread) -> bool| spreads.iter().filter(|s| of(s)).count();
        let forks = snap.forks().into_iter().map(|view| ForkReport {
            from: block(view.fork.from),
            tip: block(view.fork.tip),
            cumulative_work: view.fork.cumulative_work.to_string(),
            folded: view.folded.map(block),
        });
        Self {
            seq: snap.seq(),
            tips: TipsReport {
                best: tips.best.map(block),
                final_tip: tips.final_tip.map(block),
                served: tips.served.map(block),
                held_by: tips.held_by.count(),
                configured: endpoints.len(),
                synced: tips.synced,
            },
            unready: snap.unready().map(|reason| reason.label()).collect(),
            handed: progress.handed().map(u32::from),
            indexes,
            validators,
            alarms: AlarmsReport {
                partitioned: alarms.partitioned(),
                eclipsed: alarms.eclipsed(),
                finality_paused: alarms.finality_paused(),
                stale: named(alarms.stale()),
                ending: named(alarms.ending()),
            },
            mempool: MempoolCounts {
                transactions: spreads.len(),
                verified: count(&|spread| spread.trusted.seen > 0),
                ours_unverified: count(&|spread| spread.ours && spread.trusted.seen == 0),
                fully_spread: count(&|spread| readers > 0 && spread.trusted.seen == readers),
                trusted_readers: readers,
            },
            forks: forks.collect(),
        }
    }
}

fn block(at: BlockRef) -> Block {
    Block { height: u32::from(at.height), hash: at.hash.to_string() }
}

/// `# HELP` registrations for every gauge [`emit_gauges`] sets
pub fn describe_metrics() {
    use metrics::describe_gauge;

    describe_gauge!(names::BEST_TIP, "Height of the header chain's most-work verified block");
    describe_gauge!(
        names::FETCH_HEIGHT,
        "Highest height handed to the indexes (folded, or sent final unfolded); rewinds on a reorg"
    );
    describe_gauge!(
        names::INDEX_SYNCED,
        "1 = served at the verified tip, 0 = the served tip trails it (still syncing), by index"
    );
    describe_gauge!(
        names::ENDPOINT_STATE,
        "1 on the endpoint's health as of its last poll, by endpoint"
    );
    describe_gauge!(
        names::AGREEMENT,
        "1 on where the endpoint's chain stands against the verified best block, by endpoint"
    );
    describe_gauge!(names::TIP_HEIGHT, "Endpoint's own best-chain tip height, by endpoint");
    describe_gauge!(
        names::STALE_BLOCKS,
        "Blocks the endpoint's tip trails its own clock-based network estimate, by endpoint"
    );
    describe_gauge!(names::PEERS, "Endpoint's getpeerinfo connections, by endpoint and direction");
    describe_gauge!(names::PUSH_STREAM, "1 while the endpoint's indexer push streams are up");
    describe_gauge!(
        names::RELEASE,
        "1 on the endpoint's release, by endpoint, build and user agent"
    );
    describe_gauge!(
        names::END_OF_SERVICE_HEIGHT,
        "Height past which the endpoint's release halts (mainnet; absent = not enforced/unknown)"
    );
    describe_gauge!(
        names::TIP_HOLDERS,
        "Trusted validators holding the verified best block (0 = no tip: nothing served)"
    );
    describe_gauge!(names::BEST_HEIGHT, "Height of the header chain's most-work verified block");
    describe_gauge!(
        names::FINALITY_PAUSED,
        "1 while the final boundary waits for a trusted validator to hold it"
    );
    describe_gauge!(
        names::SHARED_OUTBOUND_MIN,
        "Fewest outbound peers any two live endpoints share; 0 = possible partition"
    );
    describe_gauge!(
        names::MEMPOOL_TRANSACTIONS,
        "Held mempool transactions, by state (verified = a trusted listing; ours_unverified = our \
         relay, unlisted)"
    );
}

/// Every gauge from `snap` + `progress`, set at scrape (decision 3: fresh, no mirror tasks)
pub fn emit_gauges<V: View>(snap: &Snapshot<V>, progress: &SyncProgress) {
    let tips = snap.tips();
    if let Some(best) = tips.best {
        metrics::gauge!(names::BEST_TIP).set(f64::from(u32::from(best.height)));
    }
    if let Some(handed) = progress.handed() {
        metrics::gauge!(names::FETCH_HEIGHT).set(f64::from(u32::from(handed)));
    }
    for (kind, _) in snap.indexed().into_iter().flat_map(|indexed| indexed.durable()) {
        metrics::gauge!(names::INDEX_SYNCED, "index" => kind.name()).set(f64::from(tips.synced));
    }
    chain_view_gauges(snap.view());
}

/// `zaino.chainview.*`: per endpoint, then the view's
fn chain_view_gauges(view: &ChainViewSnapshot) {
    let spreads = view.spreads();
    let verified = spreads.filter(|(_, spread)| spread.trusted.seen > 0).count();
    let held = view.spreads().count();
    metrics::gauge!(names::MEMPOOL_TRANSACTIONS, "state" => "verified").set(verified as f64);
    metrics::gauge!(names::MEMPOOL_TRANSACTIONS, "state" => "ours_unverified")
        .set((held - verified) as f64);

    for meta in view.endpoints().iter() {
        let endpoint = meta.address.clone();
        for state in STATES {
            let labels = [("endpoint", endpoint.clone()), ("state", state.label().to_owned())];
            metrics::gauge!(names::ENDPOINT_STATE, &labels).set(f64::from(meta.health == state));
        }
        for agreement in AGREEMENTS {
            let label = agreement.label().to_owned();
            let labels = [("endpoint", endpoint.clone()), ("agreement", label)];
            metrics::gauge!(names::AGREEMENT, &labels).set(f64::from(meta.agreement == agreement));
        }
        if let Some(tip) = meta.tip() {
            metrics::gauge!(names::TIP_HEIGHT, "endpoint" => endpoint.clone())
                .set(f64::from(u32::from(tip.height)));
        }
        if let Some(behind) = meta.stale_blocks() {
            metrics::gauge!(names::STALE_BLOCKS, "endpoint" => endpoint.clone())
                .set(f64::from(behind));
        }
        let inbound = meta.peers.iter().filter(|peer| peer.inbound).count();
        for (direction, count) in [("inbound", inbound), ("outbound", meta.peers.len() - inbound)] {
            metrics::gauge!(names::PEERS, "endpoint" => endpoint.clone(), "direction" => direction)
                .set(count as f64);
        }
        metrics::gauge!(names::PUSH_STREAM, "endpoint" => endpoint.clone())
            .set(f64::from(meta.streaming));
        if let Some(release) = &meta.release {
            let labels = [
                ("endpoint", endpoint.clone()),
                ("build", release.build.clone()),
                ("user_agent", release.user_agent.clone()),
            ];
            metrics::gauge!(names::RELEASE, &labels).set(1.0);
            if let types::EndOfService::At { height, .. } = release.end_of_service {
                metrics::gauge!(names::END_OF_SERVICE_HEIGHT, "endpoint" => endpoint)
                    .set(f64::from(u32::from(height)));
            }
        }
    }
    metrics::gauge!(names::TIP_HOLDERS).set(view.held_by().count() as f64);
    if let Some(best) = view.best() {
        metrics::gauge!(names::BEST_HEIGHT).set(f64::from(u32::from(best.height)));
    }
    if let Some(shared) = view.shared_outbound_min() {
        metrics::gauge!(names::SHARED_OUTBOUND_MIN).set(shared as f64);
    }
    metrics::gauge!(names::FINALITY_PAUSED).set(f64::from(view.alarms().finality_paused()));
}

/// Edge logs between two consecutive publishes: `synced`, then each alarm that rose or cleared
pub(crate) fn transitions<V>(prev: &Snapshot<V>, next: &Snapshot<V>) {
    let height = next.tips().served.map_or(0, |tip| u32::from(tip.height));
    match (prev.tips().synced, next.tips().synced) {
        (false, true) => info!(height, "Serving the verified tip"),
        (true, false) => info!(height, "Behind the verified tip, syncing"),
        _ => {}
    }
    alarm_edges(prev.view(), next.view());
}

fn alarm_edges(prev: &ChainViewSnapshot, next: &ChainViewSnapshot) {
    let (was, now) = (prev.alarms(), next.alarms());
    for (position, meta) in next.endpoints().iter().enumerate() {
        let Some(index) = ValidatorId::new(position) else { continue };
        let tip = meta.tip().map(|tip| u32::from(tip.height));
        let behind = meta.stale_blocks();
        match (was.stale().contains(index), now.stale().contains(index)) {
            (false, true) => warn!(
                endpoint = %meta.address, ?tip, ?behind,
                "Validator tip stale against its own clock (stalled or eclipsed)"
            ),
            (true, false) => info!(endpoint = %meta.address, ?tip, "Validator tip fresh again"),
            _ => {}
        }
        let left = meta.blocks_to_end_of_service();
        let build = meta.release.as_ref().map(|release| release.build.as_str());
        match (was.ending().contains(index), now.ending().contains(index)) {
            (false, true) => warn!(
                endpoint = %meta.address, ?build, ?left,
                "Validator release reaches end of service soon (it halts there): upgrade it"
            ),
            (true, false) => info!(endpoint = %meta.address, ?build, "Validator release upgraded"),
            _ => {}
        }
    }
    match (was.partitioned(), now.partitioned()) {
        (false, true) => warn!("Two live validators share no outbound peer (possible partition)"),
        (true, false) => info!("Live validators share outbound peers again"),
        _ => {}
    }
    match (was.eclipsed(), now.eclipsed()) {
        (false, true) => {
            warn!("Live validators reach few distinct outbound peers (possible eclipse)")
        }
        (true, false) => info!("Live validators reach enough distinct outbound peers again"),
        _ => {}
    }
    let best = next.best().map(|best| u32::from(best.height));
    match (was.finality_paused(), now.finality_paused()) {
        (false, true) => warn!(?best, "Finality paused: no trusted validator holds the boundary"),
        (true, false) => info!(?best, "Finality resumed"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status page contract: one `status` tag per end-of-service case, fields only where they exist
    #[test]
    fn a_release_serializes_its_end_of_service_by_status() {
        let release = |end_of_service| {
            let release = Release {
                build: "v6.4.2".to_owned(),
                user_agent: "/Zebra:6.4.2/".to_owned(),
                protocol_version: 170_140,
                end_of_service,
            };
            serde_json::to_value(release).expect("serializes")
        };
        let mainnet = EndOfService::At {
            height: 3_564_960,
            estimated_unix: 1_790_000_000,
            blocks_left: Some(4_960),
        };
        assert_eq!(
            release(mainnet),
            serde_json::json!({
                "build": "v6.4.2", "user_agent": "/Zebra:6.4.2/", "protocol_version": 170_140,
                "end_of_service": {
                    "status": "at", "height": 3_564_960, "estimated_unix": 1_790_000_000,
                    "blocks_left": 4_960,
                },
            })
        );
        let status = |eos| release(eos)["end_of_service"].clone();
        assert_eq!(
            status(EndOfService::NotEnforced),
            serde_json::json!({ "status": "not_enforced" })
        );
        assert_eq!(status(EndOfService::Unknown), serde_json::json!({ "status": "unknown" }));
    }
}
