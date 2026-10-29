//! Operator hold: `zainod hold` / `zainod resume`
//!
//! - marker `<serve.state_dir>/held` = the hold (survives restart, deploy, reboot)
//! - stopped zainod never held, only absent
//! - the running zainod polls it every second (`POLL`): held = state `held` (outranks every
//!   other), then after `[serve] hold_goaway_delay_secs` every gRPC connection, open or accepted
//!   later, GOAWAY'd ([`zaino_grpc::Retire`])
//! - resume = normal evaluation again
//! - only drain before a stop (a SIGTERM alone closes the listener at once): `zainod hold`
//!   returns once the GOAWAYs went out
//! - no write endpoint: holding takes the state dir, not network reach to the admin listener

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use super::{remove_marker, write_marker, MarkerError};
use crate::health::{DaemonReason, DaemonStatus};

/// Marker check cadence (a `zainod hold` lands within this)
const POLL: Duration = Duration::from_secs(1);

const MARKER: &str = "held";

/// How long `zainod hold` / `resume` waits for the running zainod to report the new state
const CONFIRM_WITHIN: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub(crate) enum Action {
    /// Take this node out of rotation until `zainod resume`, across restarts and reboots.
    ///
    /// Writes a marker in `[serve] state_dir`. The running zainod picks it up within a second:
    /// its state becomes `held` (`/readyz` 503), and after `[serve] hold_goaway_delay_secs` every
    /// gRPC connection, open or opened later, is sent GOAWAY. It keeps answering health and admin
    /// requests. A stopped zainod is never reported held. Hold before stopping a node behind a
    /// load balancer: a shutdown signal alone closes the listener at once.
    ///
    /// Prints `held`, then on stderr what changed. With `[metrics] listen_address` set, waits
    /// until the running zainod reports its GOAWAYs sent. Without it, waits as long as a running
    /// zainod can take to send them. Exits 0 once the marker is in place (holding twice is a
    /// no-op), 1 when the config or state directory could not be read or written.
    Hold,
    /// Undo `zainod hold`: back to syncing or serving as the indexes dictate.
    ///
    /// Removes the marker in `[serve] state_dir`. The running zainod picks it up within a
    /// second. Prints `resumed`, then on stderr what changed and the running zainod's state.
    /// Exits 0 once the marker is gone (resuming when not held is a no-op), 1 on an error.
    Resume,
}

/// What a command found, the marker now matching what was asked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Changed {
    Held,
    AlreadyHeld,
    Resumed,
    NotHeld,
}

/// Marker written or removed (idempotent)
fn apply(state_dir: &Path, action: Action) -> Result<Changed, MarkerError> {
    match action {
        Action::Hold if state_dir.join(MARKER).exists() => Ok(Changed::AlreadyHeld),
        Action::Hold => write_marker(state_dir, MARKER, "").map(|()| Changed::Held),
        Action::Resume => match remove_marker(state_dir, MARKER)? {
            true => Ok(Changed::Resumed),
            false => Ok(Changed::NotHeld),
        },
    }
}

/// `zainod hold` / `zainod resume`: prints the resulting state
///
/// - `Ok` = marker says what was asked (hold: and a running zainod's connections GOAWAY'd, or the
///   longest that can take waited)
pub(crate) fn command(config_path: &Path, action: Action) -> Result<ExitCode, MarkerError> {
    let config = crate::config::load_config(config_path)?;
    let state_dir = &config.serve.state_dir;
    let changed = apply(state_dir, action)?;
    let (state, note) = match changed {
        Changed::Held => ("held", "hold marker written"),
        Changed::AlreadyHeld => ("held", "already held"),
        Changed::Resumed => ("resumed", "hold marker removed"),
        Changed::NotHeld => ("resumed", "was not held"),
    };
    println!("{state}");
    eprintln!("{note}: {}", state_dir.join(MARKER).display());
    let goaway_by = goaway_bound(config.serve.hold_goaway_delay);
    match (action, config.metrics.listen_address) {
        (Action::Resume, Some(admin)) => {
            match confirm(admin, CONFIRM_WITHIN, |r| r.state != "held") {
                Some(running) => eprintln!("zainod at {admin} reports {}", running.state),
                None => eprintln!("no zainod answering at {admin}: applies when it next starts"),
            }
        }
        (Action::Resume, None) => {
            eprintln!(
                "no [metrics] listen_address to confirm on: a running zainod resumes within 1 s"
            )
        }
        (Action::Hold, Some(admin)) => {
            let goaway_sent = Some(DaemonReason::GoawaySent.label());
            let sent = |running: &Readyz| running.reason() == goaway_sent;
            match confirm(admin, goaway_by + CONFIRM_WITHIN, sent) {
                Some(running) if sent(&running) => eprintln!(
                    "zainod at {admin} reports held, every gRPC connection sent GOAWAY: safe to stop"
                ),
                Some(running) => eprintln!(
                    "zainod at {admin} reports {} ({}): GOAWAY not confirmed",
                    running.state,
                    running.reason().unwrap_or("no reason")
                ),
                None => eprintln!("no zainod answering at {admin}: applies when it next starts"),
            }
        }
        (Action::Hold, None) => {
            eprintln!(
                "no [metrics] listen_address to confirm on: waiting {}s, the longest a running \
                 zainod takes to GOAWAY every gRPC connection",
                goaway_by.as_secs()
            );
            std::thread::sleep(goaway_by);
            eprintln!("a running zainod has sent every gRPC connection GOAWAY: safe to stop");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Longest from now until any running zainod has GOAWAY'd under the hold: it reads the marker
/// within a [`POLL`] (written now, or read at its own start), sends `delay` later (+ a poll of
/// scheduling slack)
fn goaway_bound(delay: Duration) -> Duration {
    POLL + delay + POLL
}

/// `/readyz` as `zainod hold` / `resume` read it (`detail.reason` = down, degraded or held only)
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
struct Readyz {
    state: String,
    detail: Option<ReadyzDetail>,
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
struct ReadyzDetail {
    reason: Option<String>,
}

impl Readyz {
    fn reason(&self) -> Option<&str> {
        self.detail.as_ref()?.reason.as_deref()
    }
}

/// Running zainod's `/readyz` once `done` (or the last one seen after `within`)
///
/// - `None` = nothing answering
fn confirm(admin: SocketAddr, within: Duration, done: impl Fn(&Readyz) -> bool) -> Option<Readyz> {
    let admin = match admin.ip().is_unspecified() {
        true => SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), admin.port()),
        false => admin,
    };
    let deadline = std::time::Instant::now() + within;
    loop {
        let running = readyz(admin)?;
        if done(&running) || std::time::Instant::now() >= deadline {
            return Some(running);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One `GET /readyz` over plain HTTP/1.1
fn readyz(admin: SocketAddr) -> Option<Readyz> {
    use std::io::{Read as _, Write as _};

    let mut stream = std::net::TcpStream::connect_timeout(&admin, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let body = response.split("\r\n\r\n").nth(1)?;
    serde_json::from_str(body).ok()
}

/// For the process's life: the marker every [`POLL`] → the status's hold (unreadable = last
/// state kept, warned)
pub(crate) fn spawn(status: Arc<DaemonStatus>, state_dir: PathBuf, drain_delay: Duration) {
    tokio::spawn(async move {
        let marker = state_dir.join(MARKER);
        let mut held = false;
        let mut tick = tokio::time::interval(POLL);
        loop {
            tick.tick().await;
            let now = match marker.try_exists() {
                Ok(now) => now,
                Err(error) => {
                    warn!(%error, marker = %marker.display(), "Hold marker unreadable, state kept");
                    continue;
                }
            };
            if now == held {
                continue;
            }
            held = now;
            if held {
                status.retire().hold(drain_delay);
                warn!(delay = ?drain_delay, "Held by operator: /readyz 503, GOAWAY after the delay");
            } else {
                status.retire().resume();
                info!("Resumed by operator");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - Hold twice = held once, resume twice = resumed once
    /// - Marker = the whole state, kept in a state dir created on first hold
    #[test]
    fn hold_and_resume_are_idempotent_on_the_marker() {
        let root = tempfile::tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        #[rustfmt::skip]
        let commands = [
            // case,                               action,          changed,              marker after
            ("resume, no state dir yet",           Action::Resume,  Changed::NotHeld,     false),
            ("hold: state dir + marker created",   Action::Hold,    Changed::Held,        true),
            ("hold again",                         Action::Hold,    Changed::AlreadyHeld, true),
            ("resume: marker removed",             Action::Resume,  Changed::Resumed,     false),
            ("resume again",                       Action::Resume,  Changed::NotHeld,     false),
        ];
        for (case, action, changed, marked) in commands {
            assert_eq!(apply(&state_dir, action).expect("applies"), changed, "{case}");
            assert_eq!(state_dir.join(MARKER).exists(), marked, "{case}: marker present");
        }
    }

    /// A running zainod (paused clock, delay 155 s) with a marker already there at start:
    /// - held from its first poll: `/readyz` held + "GOAWAY pending", then "GOAWAY sent" by the
    ///   CLI's bound from that moment (the marker's age never shortens it)
    /// - marker removed: resumed within a poll, the hold off
    #[tokio::test(start_paused = true)]
    async fn a_marker_holds_a_running_zainod_and_its_goaways_land_within_the_cli_bound() {
        let delay = Duration::from_secs(155);
        let root = tempfile::tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        apply(&state_dir, Action::Hold).expect("holds");
        let status = Arc::new(DaemonStatus::new(zcash_protocol::consensus::NetworkType::Main, []));
        let readyz = |status: &DaemonStatus| {
            let body = status.readiness_json().1;
            serde_json::from_str::<Readyz>(&body).expect("readyz body")
        };
        let held = |reason: &str| Readyz {
            state: "held".to_owned(),
            detail: Some(ReadyzDetail { reason: Some(reason.to_owned()) }),
        };

        spawn(Arc::clone(&status), state_dir.clone(), delay);
        tokio::time::sleep(POLL).await;
        assert_eq!(readyz(&status), held("goaway_pending"), "held at the first poll");
        tokio::time::sleep(goaway_bound(delay).checked_sub(POLL).expect("bound > a poll")).await;
        assert_eq!(readyz(&status), held("goaway_sent"), "GOAWAY by the CLI's bound");

        apply(&state_dir, Action::Resume).expect("resumes");
        tokio::time::sleep(POLL).await;
        assert_eq!(status.retire().state(), zaino_grpc::Hold::Off, "resumed within a poll");
        let unbooted = Readyz { state: "provisioning".to_owned(), detail: None };
        assert_eq!(readyz(&status), unbooted, "resumed: the state rules again (not booted)");
    }
}
