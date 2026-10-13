//! Operator hold: `zainod hold` / `zainod resume`
//!
//! - marker `<serve.state_dir>/held` = the hold (survives restart, deploy, reboot; a stopped
//!   zainod is never held, only absent)
//! - the running zainod polls it every [`POLL`]: held = state `held` (outranks every other), then
//!   after `[serve] hold_goaway_delay_secs` every open gRPC connection GOAWAY'd; resume = normal
//!   evaluation again
//! - only drain before a stop (a SIGTERM alone closes the listener at once): `zainod hold`
//!   returns once the GOAWAYs went out
//! - no write endpoint: holding takes the state dir, not network reach to the admin listener

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use tracing::{info, warn};

/// Marker check cadence (a `zainod hold` lands within this)
const POLL: Duration = Duration::from_secs(1);

const MARKER: &str = "held";

/// How long `zainod hold` / `resume` waits for the running zainod to report the new state
const CONFIRM_WITHIN: Duration = Duration::from_secs(5);

/// The gRPC service's, once it exists (a hold before boot has no connection to retire)
static RETIRE: OnceLock<zaino_grpc::Retire> = OnceLock::new();

pub(crate) fn serving(retire: zaino_grpc::Retire) {
    let _ = RETIRE.set(retire);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Hold,
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

/// Marker written (synced, with its directory) or removed; idempotent
fn apply(state_dir: &Path, action: Action) -> io::Result<Changed> {
    let marker = state_dir.join(MARKER);
    let changed = match action {
        Action::Hold => {
            if marker.try_exists()? {
                return Ok(Changed::AlreadyHeld);
            }
            std::fs::create_dir_all(state_dir)?;
            std::fs::File::create(&marker)?.sync_all()?;
            Changed::Held
        }
        Action::Resume => match std::fs::remove_file(&marker) {
            Ok(()) => Changed::Resumed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Changed::NotHeld),
            Err(error) => return Err(error),
        },
    };
    // the marker's own entry durable: a hold must survive a reboot right after the command
    std::fs::File::open(state_dir)?.sync_all()?;
    Ok(changed)
}

/// `zainod hold` / `zainod resume`: prints the resulting state; exit 0 = the marker says what was
/// asked (hold: and a running zainod's connections GOAWAY'd), 1 = the config or state dir could
/// not be read or written
pub fn command(config_path: &Path, action: Action) -> i32 {
    let config = match crate::config::load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let state_dir = &config.serve.state_dir;
    let changed = match apply(state_dir, action) {
        Ok(changed) => changed,
        Err(error) => {
            eprintln!("Error: {}: {error}", state_dir.join(MARKER).display());
            return 1;
        }
    };
    let (state, note) = match changed {
        Changed::Held => ("held", "hold marker written"),
        Changed::AlreadyHeld => ("held", "already held"),
        Changed::Resumed => ("resumed", "hold marker removed"),
        Changed::NotHeld => ("resumed", "was not held"),
    };
    println!("{state}");
    eprintln!("{note}: {}", state_dir.join(MARKER).display());
    let held = action == Action::Hold;
    let maybe_running = match config.metrics.listen_address {
        Some(admin) => match confirm(admin, held) {
            Some(running) => {
                eprintln!("zainod at {admin} reports {running}");
                true
            }
            None => {
                eprintln!("no zainod answering at {admin}: applies when it next starts");
                false
            }
        },
        None => {
            eprintln!(
                "no [metrics] listen_address to confirm on: a running zainod applies it within 1 s"
            );
            true
        }
    };
    if held && maybe_running {
        let goaway_by = match goaway_deadline(state_dir, config.serve.hold_goaway_delay()) {
            Ok(goaway_by) => goaway_by,
            Err(error) => {
                eprintln!("Error: {}: {error}", state_dir.join(MARKER).display());
                return 1;
            }
        };
        if let Ok(remaining) = goaway_by.duration_since(std::time::SystemTime::now()) {
            eprintln!("waiting {}s for open gRPC connections' GOAWAY", remaining.as_secs());
            std::thread::sleep(remaining);
        }
        eprintln!("open gRPC connections sent GOAWAY: safe to stop");
    }
    0
}

/// When a running zainod has GOAWAY'd every connection under this hold: marker written + its poll
/// + `delay` (+ one more poll for scheduling slack); an old marker = already past
fn goaway_deadline(state_dir: &Path, delay: Duration) -> io::Result<std::time::SystemTime> {
    let written = std::fs::metadata(state_dir.join(MARKER))?.modified()?;
    Ok(written + POLL + delay + POLL)
}

/// The running zainod's `/readyz` state once it matches `held` (or the last one seen after
/// [`CONFIRM_WITHIN`]); `None` = nothing answering
fn confirm(admin: SocketAddr, held: bool) -> Option<String> {
    let admin = match admin.ip().is_unspecified() {
        true => SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), admin.port()),
        false => admin,
    };
    let deadline = std::time::Instant::now() + CONFIRM_WITHIN;
    loop {
        let state = readyz_state(admin)?;
        if (state == "held") == held || std::time::Instant::now() >= deadline {
            return Some(state);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One `GET /readyz` over plain HTTP/1.1: its `state`
fn readyz_state(admin: SocketAddr) -> Option<String> {
    use std::io::{Read as _, Write as _};

    let mut stream = std::net::TcpStream::connect_timeout(&admin, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let body = response.split("\r\n\r\n").nth(1)?;
    let readiness: serde_json::Value = serde_json::from_str(body).ok()?;
    readiness["state"].as_str().map(str::to_owned)
}

/// For the process's life: the marker every [`POLL`] (unreadable = last state kept, warned)
pub(crate) fn spawn(state_dir: std::path::PathBuf, drain_delay: Duration) {
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
            crate::status::hold(held);
            if !held {
                info!("Resumed by operator");
                continue;
            }
            warn!(delay = ?drain_delay, "Held by operator: /readyz 503 until `zainod resume`");
            tokio::spawn(async move {
                tokio::time::sleep(drain_delay).await;
                if let (true, Some(retire)) = (crate::status::held(), RETIRE.get()) {
                    retire.retire_all();
                    info!("Held: every open gRPC connection sent GOAWAY");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hold twice = held once; resume twice = resumed once; the marker is the whole state, kept
    /// in a state dir created on first hold
    #[test]
    fn hold_and_resume_are_idempotent_on_the_marker() {
        let root = tempfile::tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        let steps = [
            (Action::Resume, Changed::NotHeld, false),
            (Action::Hold, Changed::Held, true),
            (Action::Hold, Changed::AlreadyHeld, true),
            (Action::Resume, Changed::Resumed, false),
            (Action::Resume, Changed::NotHeld, false),
        ];
        for (step, (action, changed, marked)) in steps.into_iter().enumerate() {
            assert_eq!(apply(&state_dir, action).expect("applies"), changed, "step {step}");
            assert_eq!(state_dir.join(MARKER).exists(), marked, "step {step}: marker");
        }

        // GOAWAY deadline = the marker's write time + its poll + the delay + slack; none unheld
        let delay = Duration::from_secs(155);
        assert!(goaway_deadline(&state_dir, delay).is_err(), "deadline without a marker");
        apply(&state_dir, Action::Hold).expect("holds");
        let written = std::fs::metadata(state_dir.join(MARKER)).and_then(|m| m.modified());
        let deadline = goaway_deadline(&state_dir, delay).expect("marker readable");
        let after = deadline.duration_since(written.expect("mtime")).expect("deadline after write");
        assert_eq!(after, delay + POLL + POLL);
    }
}
