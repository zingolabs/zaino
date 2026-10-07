//! Reporting from one snapshot (G9): `/statusz` ([`Report`]), gauges at scrape ([`emit_gauges`]),
//! edge logs per publish ([`transitions`])
//!
//! - State = f(snapshot, `handed`): nothing mirrored into gauges or watches between scrapes
//! - Gauge names = ztest's `zainod` families (a rename breaks its sync probes)

use serde::Serialize;
use tracing::info;
use zaino_chainview::{EndpointSet, Spread};
use zaino_nfs::INDEXES;
use zaino_persistence::{IndexKind, View};
use zaino_primitives::types::{self, BlockRef, Height};

use crate::snapshot::{Snapshot, Unready};

mod names {
    pub(super) const BEST_TIP: &str = "zaino.best_tip";
    pub(super) const FETCH_HEIGHT: &str = "zaino.fetch_height";
    pub(super) const INDEX_FINALIZED_HEIGHT: &str = "zaino.index.finalized_height";
    pub(super) const INDEX_SYNCED: &str = "zaino.index.synced";
}

/// `/statusz` body (zainod adds its process fields beside it)
///
/// - `handed` = last block handed to the indexes; `indexes` empty before the NFS's first publish
#[derive(Debug, Serialize, PartialEq)]
pub struct Report {
    seq: u64,
    tips: TipsReport,
    unready: Vec<Unready>,
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

#[derive(Debug, Serialize, PartialEq)]
struct IndexReport {
    name: &'static str,
    enabled: bool,
    durable: Option<u32>,
}

/// Chain view facts only (health, latency, failures: the traffic balancer's `MemberTable`)
#[derive(Debug, Serialize, PartialEq)]
struct ValidatorReport {
    address: String,
    agreement: &'static str,
    height: Option<u32>,
    stale_blocks: Option<u32>,
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
    pub fn of<V: View>(snap: &Snapshot<V>, handed: Option<Height>) -> Self {
        let (tips, view) = (snap.tips(), snap.view());
        let endpoints = view.endpoints();
        let named = |set: EndpointSet| -> Vec<String> {
            let at = set.positions().filter_map(|position| endpoints.get(position));
            at.map(|meta| meta.address.clone()).collect()
        };
        let indexes = snap.indexed().map_or_else(Vec::new, |indexed| {
            let durable: Vec<_> = indexed.durable().collect();
            let report = |kind: IndexKind| {
                let tip = durable.iter().find(|(each, _)| *each == kind).map(|(_, tip)| *tip);
                let durable = tip.flatten().map(|tip| u32::from(tip.height));
                IndexReport { name: kind.name(), enabled: tip.is_some(), durable }
            };
            INDEXES.into_iter().map(report).collect()
        });
        let validators = endpoints
            .iter()
            .map(|meta| ValidatorReport {
                address: meta.address.clone(),
                agreement: meta.agreement.label(),
                height: meta.tip().map(|tip| u32::from(tip.height)),
                stale_blocks: meta.stale_blocks(),
                streaming: meta.streaming,
                release: meta.release.as_ref().map(|release| Release {
                    build: release.build.clone(),
                    user_agent: release.user_agent.clone(),
                    protocol_version: release.protocol_version,
                    end_of_service: match release.end_of_service {
                        types::EndOfService::At { height, estimated_unix } => EndOfService::At {
                            height: u32::from(height),
                            estimated_unix,
                            blocks_left: meta.blocks_to_end_of_service(),
                        },
                        types::EndOfService::NotEnforced => EndOfService::NotEnforced,
                        types::EndOfService::Unknown => EndOfService::Unknown,
                    },
                }),
                peers: meta
                    .peers
                    .iter()
                    .map(|peer| Peer { addr: peer.addr.clone(), inbound: peer.inbound })
                    .collect(),
            })
            .collect();
        let alarms = view.alarms();
        let readers = view.mempool_readers().count();
        let spreads: Vec<_> = view.spreads().map(|(_, spread)| spread).collect();
        let count = |of: fn(&Spread) -> bool| spreads.iter().filter(|spread| of(spread)).count();
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
            unready: snap.unready().collect(),
            handed: handed.map(u32::from),
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
                verified: count(|spread| spread.trusted.seen > 0),
                ours_unverified: count(|spread| spread.ours && spread.trusted.seen == 0),
                fully_spread: spreads
                    .iter()
                    .filter(|spread| readers > 0 && spread.trusted.seen == readers)
                    .count(),
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
        names::INDEX_FINALIZED_HEIGHT,
        "Highest height the index has durably written, by index"
    );
    describe_gauge!(
        names::INDEX_SYNCED,
        "1 = served at the verified tip, 0 = the served tip trails it (still syncing), by index"
    );
}

/// Every gauge from `snap` + `handed`, set at scrape (decision 3: fresh, no mirror tasks)
pub fn emit_gauges<V: View>(snap: &Snapshot<V>, handed: Option<Height>) {
    let tips = snap.tips();
    if let Some(best) = tips.best {
        metrics::gauge!(names::BEST_TIP).set(f64::from(u32::from(best.height)));
    }
    if let Some(handed) = handed {
        metrics::gauge!(names::FETCH_HEIGHT).set(f64::from(u32::from(handed)));
    }
    for (kind, durable) in snap.indexed().into_iter().flat_map(|indexed| indexed.durable()) {
        let index = kind.name();
        if let Some(durable) = durable {
            metrics::gauge!(names::INDEX_FINALIZED_HEIGHT, "index" => index)
                .set(f64::from(u32::from(durable.height)));
        }
        metrics::gauge!(names::INDEX_SYNCED, "index" => index).set(f64::from(tips.synced));
    }
}

/// Edge logs between two consecutive publishes (`synced` opened or closed)
pub(crate) fn transitions<V>(prev: &Snapshot<V>, next: &Snapshot<V>) {
    let (was, now) = (prev.tips().synced, next.tips().synced);
    let height = next.tips().served.map_or(0, |tip| u32::from(tip.height));
    match (was, now) {
        (false, true) => info!(height, "Serving the verified tip"),
        (true, false) => info!(height, "Behind the verified tip, syncing"),
        _ => {}
    }
}
