// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd monitor` — an external health monitor for a running recorder.
//!
//! The SCM's failure actions only fire when the process *dies*. A recorder that
//! is alive but no longer recording — a tuner whose supervised unit exhausted
//! its restart budget, an SDR that enumerates but never locks the control
//! channel again, a panel that stopped answering — looks perfectly healthy to
//! Windows and produces silence. This mode is the outermost loop: a separate,
//! deliberately tiny process of the *same* executable that watches the panel
//! and restarts the recorder when it stops doing its job.
//!
//! It is tiny on purpose. It opens no SDR, runs no DSP, loads no SoapySDR: it
//! speaks HTTP to `GET /api/status` and, on Windows, to the SCM. If the monitor
//! itself wedges, the worst case is that nothing gets restarted — it can never
//! be the thing that breaks a recording.
//!
//! # Shape
//!
//! [`HealthTracker`] is a pure state machine: [`Observation`]s and an `Instant`
//! go in, a [`Verdict`] comes out. Every rule, every duration threshold and the
//! whole restart backoff live there and are unit-tested. HTTP and the SCM are
//! thin adapters around it, because those are the parts that cannot be tested
//! without a live box.
//!
//! # No event log
//!
//! Restarts are recorded in the rolling log file only. `windows-service`
//! exposes no event-log API, and registering an event source by hand is a
//! registry write plus a `ReportEventW` binding — more moving parts than the
//! log file an operator is already reading.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use serde_json::Value;
use trunkd_core::{Error, Result};
#[cfg(windows)]
use windows_service::service::{ServiceExitCode, ServiceState};

/// How soon after a restart a fresh fault counts as "that restart did not take"
/// and escalates the backoff.
const ESCALATE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How long the recorder has to stay healthy before the backoff resets.
const HEALTH_RESET: Duration = Duration::from_secs(30 * 60);

/// How long a service restart waits for the recorder to stop.
///
/// Longer than the service's own 30 s stop hint, so a recorder that is draining
/// normally is never escalated into a kill.
#[cfg(windows)]
const RESTART_STOP_TIMEOUT: Duration = Duration::from_secs(45);

/// HTTP timeout for one panel poll. Generous next to `--interval`, and short
/// enough that a hung panel still counts as a failed poll promptly.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment fallback for the panel bearer token, so an operator does not
/// have to put it in a service command line the whole box can read.
pub const TOKEN_ENV: &str = "TRUNKD_MONITOR_TOKEN";

/// The default panel URL, matching `[server] bind`'s loopback default.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// The monitor's own rolling log file.
///
/// Deliberately not the recorder's `trunkd.log`: the two are pointed at the
/// same directory by `service install --monitor`, and one file written by two
/// processes interleaves and loses lines.
pub const LOG_FILE: &str = "trunkd-monitor.log";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Options for `trunkd monitor`.
#[derive(Args, Debug, Clone)]
pub struct MonitorArgs {
    /// Base URL of the recorder's local control panel (`[server] bind`).
    #[arg(long, value_name = "URL", default_value = DEFAULT_URL)]
    pub url: String,

    /// Panel bearer token, when `[server] tokens` is set.
    ///
    /// Falls back to $TRUNKD_MONITOR_TOKEN, and then to the first
    /// `[server] tokens` entry of the recorder's `--config` file — which is
    /// what `service install --monitor` passes, so the installed monitor needs
    /// no token of its own and none sits in its command line.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Windows service to restart when the recorder is unhealthy.
    #[arg(long, value_name = "NAME")]
    pub service: Option<String>,

    /// Command to run to restart the recorder, when it is not a service.
    #[arg(long, value_name = "COMMAND", conflicts_with = "service")]
    pub restart_cmd: Option<String>,

    /// Directory for the monitor's rolling log file.
    #[arg(long, value_name = "DIR", default_value = "logs")]
    pub log_dir: PathBuf,

    /// Log verbosity (`error`|`warn`|`info`|`debug`|`trace`). `RUST_LOG` wins.
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    pub log_level: String,

    /// How often to poll the panel.
    #[arg(long, value_name = "DURATION", default_value = "10s", value_parser = parse_duration)]
    pub interval: Duration,

    /// Unhealthy after the panel has been unreachable this long.
    #[arg(long, value_name = "DURATION", default_value = "60s", value_parser = parse_duration)]
    pub unreachable_after: Duration,

    /// Unhealthy after no control-following tuner has held a lock this long.
    #[arg(long, value_name = "DURATION", default_value = "10m", value_parser = parse_duration)]
    pub unlocked_after: Duration,

    /// Unhealthy after a supervised unit has been failed this long.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_duration)]
    pub failed_after: Duration,

    /// Unhealthy after the service has been stopped this long (needs --service).
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_duration)]
    pub stopped_after: Duration,

    /// Never restart twice within this window.
    #[arg(long, value_name = "DURATION", default_value = "2m", value_parser = parse_duration)]
    pub min_interval: Duration,

    /// Upper bound on the doubling restart backoff.
    #[arg(long, value_name = "DURATION", default_value = "30m", value_parser = parse_duration)]
    pub max_interval: Duration,
}

/// Parse `10s` / `2m` / `1h` / `500ms`, or a bare number of seconds.
///
/// A CLI that took raw seconds would put `--unlocked-after 600` in a service
/// command line nobody can read six months later.
fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("'{s}' is not a duration like 30s, 10m or 1h"))?;
    let d = match unit.trim() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        other => {
            return Err(format!(
                "unknown duration unit '{other}'; use ms, s, m or h"
            ))
        }
    };
    Ok(d)
}

/// The command line `trunkd service install --monitor` registers.
///
/// The same command line has to work from a terminal, so there is no
/// "I am a service" flag: the monitor attaches to the SCM if there is one and
/// runs in the foreground if there is not.
///
/// `--config` is the recorder's own config file. The monitor reads nothing
/// from it but the panel token ([`resolve_token`]): with `[server] tokens`
/// set, every `/api/status` poll without one is a 401, and a monitor that read
/// that as "the panel is dead" would restart a healthy recorder forever.
#[cfg_attr(not(windows), allow(dead_code))] // only `service install` builds it
pub fn service_arguments(
    recorder: &str,
    url: &str,
    log_dir: &Path,
    config: &Path,
) -> Vec<OsString> {
    vec![
        OsString::from("monitor"),
        OsString::from("--service"),
        OsString::from(recorder),
        OsString::from("--url"),
        OsString::from(url),
        OsString::from("--log-dir"),
        log_dir.as_os_str().to_os_string(),
        OsString::from("--config"),
        config.as_os_str().to_os_string(),
    ]
}

/// The bearer token the monitor presents to the panel, if any.
///
/// In order: `--token`, then `$TRUNKD_MONITOR_TOKEN`, then the first
/// `[server] tokens` entry of the recorder's config file (loaded exactly as
/// the recorder loads it, `TRUNKD_` environment overrides included, so the
/// two cannot disagree about which tokens exist). Blank values count as
/// absent. A config that cannot be read is logged and means "no token": the
/// monitor still watches, and a panel with no tokens set needs none.
pub fn resolve_token(
    flag: Option<&str>,
    env: Option<&str>,
    config: Option<&Path>,
) -> Option<String> {
    let given = |t: Option<&str>| {
        t.map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    if let Some(t) = given(flag).or_else(|| given(env)) {
        return Some(t);
    }
    let path = config?;
    match trunkd::config::Config::load(path) {
        Ok(cfg) => cfg
            .server
            .tokens
            .iter()
            .map(|t| t.trim())
            .find(|t| !t.is_empty())
            .map(str::to_owned),
        Err(e) => {
            tracing::warn!(
                config = %path.display(),
                error = %e,
                "could not read the panel token from the recorder's config; polling without one"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The health state machine (pure).
// ---------------------------------------------------------------------------

/// What the SCM says about the recorder service, when there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    /// Running, or on its way there.
    #[cfg_attr(not(windows), allow(dead_code))] // only the SCM probe produces it
    Up,
    /// Stopped, and the SCM recorded a **clean** exit — which is what
    /// `trunkd service stop`, `sc stop` and services.msc all leave behind.
    ///
    /// The operator stopped it on purpose. The monitor must not fight them, and
    /// must not read the consequences of that stop (an unreachable panel, no
    /// control lock) as faults of their own.
    StoppedClean,
    /// Stopped or stopping after an **abnormal** exit — a non-zero Win32 code
    /// (1067 `ERROR_PROCESS_ABORTED` after a kill or a crash) or a
    /// service-specific one. The SCM's own failure actions have already had
    /// their turn and did not recover it.
    Down,
    /// No `--service`, no such service, or the query failed. Rule 4 is off.
    Unknown,
}

/// One poll's worth of what the recorder said about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observation {
    /// The panel answered 200 with a parseable body.
    pub reachable: bool,
    /// At least one control-following tuner reports a live control lock.
    ///
    /// The panel already applies its liveness gate to `locked`, so a tuner
    /// whose stream heartbeat went stale reports `false` here without the
    /// monitor having to re-derive staleness.
    pub any_locked: bool,
    /// Whether any tuner follows control at all. A voice-only or `iqfile`
    /// instance has nothing to lock, so rule 2 must not fire on it.
    pub has_control_tuners: bool,
    /// Supervised units in the terminal `failed` state.
    pub failed_units: Vec<String>,
    /// The recorder's own uptime; `None` when unreachable.
    pub uptime_s: Option<u64>,
}

/// Duration thresholds; every one is a CLI flag.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Rule 1.
    pub unreachable_after: Duration,
    /// Rule 2.
    pub unlocked_after: Duration,
    /// Rule 3.
    pub failed_after: Duration,
    /// Rule 4.
    pub stopped_after: Duration,
    /// Floor on the gap between two restarts.
    pub min_interval: Duration,
    /// Ceiling on the doubling backoff.
    pub max_interval: Duration,
}

impl From<&MonitorArgs> for Thresholds {
    fn from(a: &MonitorArgs) -> Self {
        Thresholds {
            unreachable_after: a.unreachable_after,
            unlocked_after: a.unlocked_after,
            failed_after: a.failed_after,
            stopped_after: a.stopped_after,
            min_interval: a.min_interval,
            max_interval: a.max_interval,
        }
    }
}

/// Why the monitor thinks the recorder is unhealthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Rule 4: the SCM has no process running for us.
    ServiceStopped(Duration),
    /// Rule 1: the panel has not answered.
    Unreachable(Duration),
    /// Rule 3: a supervised unit gave up.
    FailedUnit(String, Duration),
    /// Rule 2: nothing is following a control channel.
    NoLock(Duration),
}

impl Reason {
    /// The rule number, for logs an operator can grep against the docs.
    pub fn rule(&self) -> u8 {
        match self {
            Reason::Unreachable(_) => 1,
            Reason::NoLock(_) => 2,
            Reason::FailedUnit(..) => 3,
            Reason::ServiceStopped(_) => 4,
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Unreachable(d) => {
                write!(f, "the panel has not answered for {}s", d.as_secs())
            }
            Reason::NoLock(d) => write!(f, "no tuner has held a control lock for {}s", d.as_secs()),
            Reason::FailedUnit(name, d) => write!(
                f,
                "supervised unit '{name}' has been failed for {}s",
                d.as_secs()
            ),
            Reason::ServiceStopped(d) => {
                write!(f, "the service has been stopped for {}s", d.as_secs())
            }
        }
    }
}

/// What the monitor should do about this poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do.
    Healthy,
    /// The recorder is stopped because someone stopped it. Nothing to do, and
    /// nothing to measure: every rule is held down until it comes back.
    Idle,
    /// Unhealthy, but too soon after the last restart to act.
    Waiting {
        /// Why it is unhealthy.
        reason: Reason,
        /// How much longer the backoff has to run.
        remaining: Duration,
    },
    /// Restart the recorder now.
    Restart(Reason),
    /// The service is simply not running: start it, no stop needed.
    StartService(Reason),
}

/// The health rules and the restart backoff, as a pure state machine.
#[derive(Debug)]
pub struct HealthTracker {
    th: Thresholds,
    unreachable_since: Option<Instant>,
    unlocked_since: Option<Instant>,
    failed_since: Option<Instant>,
    stopped_since: Option<Instant>,
    healthy_since: Option<Instant>,
    last_uptime_s: Option<u64>,
    last_restart: Option<Instant>,
    backoff: Duration,
    /// Whether this restart cycle has already doubled the backoff.
    escalated: bool,
}

impl HealthTracker {
    /// A tracker with no history.
    pub fn new(th: Thresholds) -> Self {
        HealthTracker {
            th,
            unreachable_since: None,
            unlocked_since: None,
            failed_since: None,
            stopped_since: None,
            healthy_since: None,
            last_uptime_s: None,
            last_restart: None,
            backoff: th.min_interval,
            escalated: false,
        }
    }

    /// The backoff currently in force, for logs and tests.
    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    /// Forget every rule's window. Used when the recorder restarts underneath
    /// us: the previous process's fault history says nothing about this one.
    fn reset_windows(&mut self) {
        self.unreachable_since = None;
        self.unlocked_since = None;
        self.failed_since = None;
        self.stopped_since = None;
    }

    /// Fold one poll in and decide.
    pub fn observe(&mut self, now: Instant, obs: &Observation, svc: ServiceHealth) -> Verdict {
        // A recorder whose uptime went backwards is a *new* process — whether
        // we restarted it, the SCM did, or the operator did. Every window
        // starts again, or a 10-minute unlock timer inherited from the dead
        // process would restart the healthy one that replaced it.
        if let (Some(prev), Some(current)) = (self.last_uptime_s, obs.uptime_s) {
            if current < prev {
                self.reset_windows();
            }
        }
        if obs.uptime_s.is_some() {
            self.last_uptime_s = obs.uptime_s;
        }

        // A service the operator stopped is not a fault, and neither is
        // anything that follows from it: the panel is unreachable *because*
        // there is no process, and no tuner holds a lock *because* there is no
        // tuner. Hold every rule down and forget their windows, so the recorder
        // gets a full, fresh set of them when it is started again.
        if svc == ServiceHealth::StoppedClean {
            self.reset_windows();
            // Not "healthy" either: a stopped recorder proves nothing about the
            // restart that preceded it, so the backoff keeps whatever it had.
            self.healthy_since = None;
            self.last_uptime_s = None;
            return Verdict::Idle;
        }

        let stopped = track(&mut self.stopped_since, now, svc == ServiceHealth::Down);
        let unreachable = track(&mut self.unreachable_since, now, !obs.reachable);
        // Rule 3 keys on *any* failed unit; the name is carried for the log.
        let failed_name = obs.failed_units.first().cloned();
        let failed = track(&mut self.failed_since, now, failed_name.is_some());
        // Only meaningful while the panel is answering: an unreachable panel is
        // rule 1's business, and reporting both is noise.
        let unlocked = track(
            &mut self.unlocked_since,
            now,
            obs.reachable && obs.has_control_tuners && !obs.any_locked,
        );

        // Ordered most- to least-specific: if the process is not running, its
        // panel being unreachable and its tuners unlocked are consequences.
        let reason = over(stopped, self.th.stopped_after)
            .map(Reason::ServiceStopped)
            .or_else(|| over(unreachable, self.th.unreachable_after).map(Reason::Unreachable))
            .or_else(|| {
                over(failed, self.th.failed_after)
                    .map(|d| Reason::FailedUnit(failed_name.unwrap_or_default(), d))
            })
            .or_else(|| over(unlocked, self.th.unlocked_after).map(Reason::NoLock));

        let Some(reason) = reason else {
            let since = *self.healthy_since.get_or_insert(now);
            if now.duration_since(since) >= HEALTH_RESET {
                // A full window of health means the last restart worked; the
                // next fault deserves a fresh, short backoff.
                self.backoff = self.th.min_interval;
                self.last_restart = None;
                self.escalated = false;
            }
            return Verdict::Healthy;
        };

        self.healthy_since = None;

        if let Some(last) = self.last_restart {
            let since = now.duration_since(last);
            if since < ESCALATE_WINDOW && !self.escalated {
                // Broken again right after a restart: restarting harder will
                // not help, so wait longer before trying again.
                self.backoff = (self.backoff * 2).min(self.th.max_interval);
                self.escalated = true;
            }
            let wait = self.backoff.max(self.th.min_interval);
            if since < wait {
                return Verdict::Waiting {
                    reason,
                    remaining: wait - since,
                };
            }
        }

        match reason {
            Reason::ServiceStopped(_) => Verdict::StartService(reason),
            _ => Verdict::Restart(reason),
        }
    }

    /// Record that a restart was just performed.
    pub fn restarted(&mut self, now: Instant) {
        self.last_restart = Some(now);
        self.escalated = false;
        self.healthy_since = None;
        self.last_uptime_s = None;
        self.reset_windows();
    }
}

/// Keep a "continuously true since" mark and return how long it has held.
fn track(slot: &mut Option<Instant>, now: Instant, active: bool) -> Option<Duration> {
    if !active {
        *slot = None;
        return None;
    }
    let since = *slot.get_or_insert(now);
    Some(now.duration_since(since))
}

/// The elapsed time, if it has reached `threshold`.
fn over(elapsed: Option<Duration>, threshold: Duration) -> Option<Duration> {
    elapsed.filter(|d| *d >= threshold)
}

// ---------------------------------------------------------------------------
// Adapters: the panel, and restarting.
// ---------------------------------------------------------------------------

/// Turn a `/api/status` body into an [`Observation`].
///
/// Pure, and tolerant: a panel from a newer or older build that omits a field
/// must degrade into "nothing to report", never into a restart.
pub fn parse_status(body: &Value) -> Observation {
    let tuners = body.get("tuners").and_then(Value::as_array);
    // "Follows control" is `role`: `auto` (control and voice in one
    // channelizer) and `control`. A `voice` or `data` tuner never locks one.
    // A tuner keyup has switched off is not a tuner that failed to lock: it is
    // idling with its board open because it was told to, and no number of
    // service restarts will make it lock. Absent (an older panel that does not
    // report the field) means enabled, so nothing regresses.
    let control: Vec<&Value> = tuners
        .map(|ts| {
            ts.iter()
                .filter(|t| {
                    matches!(
                        t.get("role").and_then(Value::as_str),
                        Some("auto") | Some("control")
                    ) && t.get("enabled").and_then(Value::as_bool) != Some(false)
                })
                .collect()
        })
        .unwrap_or_default();

    let failed_units = body
        .get("supervisor")
        .and_then(Value::as_array)
        .map(|units| {
            units
                .iter()
                .filter(|u| u.get("state").and_then(Value::as_str) == Some("failed"))
                .map(|u| {
                    u.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default();

    Observation {
        reachable: true,
        any_locked: control
            .iter()
            .any(|t| t.get("locked").and_then(Value::as_bool) == Some(true)),
        has_control_tuners: !control.is_empty(),
        failed_units,
        uptime_s: body.get("uptime_s").and_then(Value::as_u64),
    }
}

/// The `GET <url>/api/status` request, with the bearer token when there is one.
fn status_request(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let endpoint = format!("{}/api/status", url.trim_end_matches('/'));
    let mut req = agent.get(&endpoint);
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    req
}

/// What one poll came back with.
#[derive(Debug, Clone, PartialEq)]
enum Poll {
    /// A status body, read.
    Status(Observation),
    /// The panel answered, and refused the monitor (401/403): the token is
    /// missing or wrong.
    Refused(u16),
    /// No usable answer.
    Failed,
}

/// Poll `GET <url>/api/status`.
fn poll(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Poll {
    match status_request(agent, url, token).call() {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(v) => Poll::Status(parse_status(&v)),
                Err(e) => {
                    tracing::warn!(error = %e, "panel returned a body that is not JSON");
                    Poll::Failed
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "could not read the panel's response");
                Poll::Failed
            }
        },
        Err(ureq::Error::StatusCode(code @ (401 | 403))) => Poll::Refused(code),
        Err(e) => {
            tracing::debug!(error = %e, url = %url, "panel poll failed");
            Poll::Failed
        }
    }
}

/// Turn a poll into what the health rules see.
///
/// A refusal is an answer: the recorder's process is up and serving its
/// panel, which is everything rule 1 asks. What it cannot say is whether the
/// tuners are locked or a unit has failed, so those rules see nothing to act
/// on — a token mismatch is a misconfiguration to fix, and no number of
/// restarts would fix it.
fn observation(poll: &Poll) -> Observation {
    match poll {
        Poll::Status(obs) => obs.clone(),
        Poll::Refused(_) => Observation {
            reachable: true,
            ..Observation::default()
        },
        Poll::Failed => Observation::default(),
    }
}

/// Classify what the SCM reports for a service.
///
/// The exit code is the whole point: the SCM records `Win32(0)` for a service
/// that was asked to stop and stopped, and a real error code for one that died.
/// Without that distinction the monitor cannot tell `trunkd service stop` from
/// a crash, and would restart the recorder out from under the operator who just
/// stopped it.
#[cfg(windows)]
fn classify_service(state: ServiceState, exit: ServiceExitCode) -> ServiceHealth {
    match state {
        ServiceState::Stopped | ServiceState::StopPending => match exit {
            ServiceExitCode::Win32(0) => ServiceHealth::StoppedClean,
            _ => ServiceHealth::Down,
        },
        _ => ServiceHealth::Up,
    }
}

/// Ask the SCM how the recorder service is doing.
#[cfg(windows)]
fn service_health(name: Option<&str>) -> ServiceHealth {
    let Some(name) = name else {
        return ServiceHealth::Unknown;
    };
    match crate::service::query_state(name) {
        Ok(Some((state, exit))) => classify_service(state, exit),
        Ok(None) => ServiceHealth::Unknown,
        Err(e) => {
            tracing::warn!(error = %e, "could not query the service state");
            ServiceHealth::Unknown
        }
    }
}

/// On Linux there is no SCM; rule 4 is simply off and `--restart-cmd` is the
/// restart path (a `systemctl restart trunkd`, typically).
#[cfg(not(windows))]
fn service_health(_name: Option<&str>) -> ServiceHealth {
    ServiceHealth::Unknown
}

/// Run the operator's `--restart-cmd` through the platform shell.
fn run_restart_cmd(cmd: &str) -> Result<()> {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    let status = c
        .status()
        .map_err(|e| Error::Other(format!("could not run --restart-cmd: {e}")))?;
    if status.success() {
        tracing::info!(command = %cmd, "restart command succeeded");
        Ok(())
    } else {
        Err(Error::Other(format!(
            "--restart-cmd exited with {status}: {cmd}"
        )))
    }
}

/// Carry out a verdict. `start_only` skips the stop half (rule 4).
#[cfg_attr(not(windows), allow(unused_variables))] // start-only is an SCM notion
fn act(args: &MonitorArgs, start_only: bool) -> Result<()> {
    match (&args.service, &args.restart_cmd) {
        #[cfg(windows)]
        (Some(name), _) => {
            if start_only {
                crate::service::start(name)
            } else {
                crate::service::restart(name, RESTART_STOP_TIMEOUT)
            }
        }
        #[cfg(not(windows))]
        (Some(_), _) => Err(Error::unsupported(
            "--service needs Windows; on Linux use --restart-cmd \
             \"systemctl restart trunkd\"",
        )),
        (None, Some(cmd)) => run_restart_cmd(cmd),
        (None, None) => Err(Error::config(
            "nothing to restart: pass --service <NAME> (Windows) or \
             --restart-cmd \"<COMMAND>\"",
        )),
    }
}

/// Run the monitor loop until `stop` is set.
///
/// `on_running` is called once the first poll is set up, so an SCM-hosted
/// monitor reports `Running` at the same point the recorder does.
///
/// `config` is the recorder's config file (the global `--config`), read only
/// for the panel token — see [`resolve_token`].
pub fn run(
    args: &MonitorArgs,
    config: Option<&Path>,
    stop: Arc<AtomicBool>,
    on_running: &mut dyn FnMut(),
) -> Result<()> {
    if args.service.is_none() && args.restart_cmd.is_none() {
        return Err(Error::config(
            "trunkd monitor needs something to restart: --service <NAME> on \
             Windows, or --restart-cmd \"<COMMAND>\"",
        ));
    }
    std::fs::create_dir_all(&args.log_dir)?;
    let _guard = trunkd::logging::init(&args.log_level, args.log_dir.to_str(), LOG_FILE);

    let env_token = std::env::var(TOKEN_ENV).ok();
    let find_token = || resolve_token(args.token.as_deref(), env_token.as_deref(), config);
    let mut token = find_token();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        url = %args.url,
        service = args.service.as_deref().unwrap_or("-"),
        restart_cmd = args.restart_cmd.as_deref().unwrap_or("-"),
        log_dir = %args.log_dir.display(),
        interval_s = args.interval.as_secs(),
        config = %config.map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()),
        authenticated = token.is_some(),
        "trunkd monitor starting"
    );
    tracing::info!(
        unreachable_after_s = args.unreachable_after.as_secs(),
        unlocked_after_s = args.unlocked_after.as_secs(),
        failed_after_s = args.failed_after.as_secs(),
        stopped_after_s = args.stopped_after.as_secs(),
        min_interval_s = args.min_interval.as_secs(),
        max_interval_s = args.max_interval.as_secs(),
        "health thresholds"
    );

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();

    let mut tracker = HealthTracker::new(Thresholds::from(args));
    // An idle recorder is a steady state that can last days; it is announced on
    // the way in and on the way out, not once per poll.
    let mut idle = false;
    // Whether the panel is refusing the monitor, so it is said once on the way
    // in and once on the way out rather than every poll.
    let mut refused = false;
    on_running();

    while !stop.load(Ordering::Relaxed) {
        let mut answer = poll(&agent, &args.url, token.as_deref());
        if let Poll::Refused(_) = answer {
            // The operator may have changed the tokens (and restarted the
            // recorder) since the monitor started: look again before saying so.
            let fresh = find_token();
            if fresh.is_some() && fresh != token {
                token = fresh;
                answer = poll(&agent, &args.url, token.as_deref());
            }
        }
        match (&answer, refused) {
            (Poll::Refused(code), false) => {
                tracing::error!(
                    status = code,
                    authenticated = token.is_some(),
                    "the panel refuses the monitor's token: the recorder is answering, but \
                     tuner and unit health cannot be read, so only rule 1 (unreachable) is \
                     watched. Give the monitor --config <trunkd.toml>, --token, or \
                     ${TOKEN_ENV} matching [server] tokens"
                );
                refused = true;
            }
            (Poll::Refused(_), true) => {}
            (_, true) => {
                tracing::info!("the panel accepts the monitor again; every health rule is back on");
                refused = false;
            }
            _ => {}
        }
        let obs = observation(&answer);
        let svc = service_health(args.service.as_deref());
        let verdict = tracker.observe(Instant::now(), &obs, svc);

        let now_idle = verdict == Verdict::Idle;
        if now_idle != idle {
            if now_idle {
                tracing::info!(
                    "recorder service is stopped cleanly (exit 0); not restarting — \
                     start it with `trunkd service start`"
                );
            } else {
                tracing::info!("recorder service is running again; health rules are back on");
            }
            idle = now_idle;
        }

        match verdict {
            // Said everything there is to say on the transition above.
            Verdict::Idle => {}
            Verdict::Healthy => tracing::debug!(
                locked = obs.any_locked,
                reachable = obs.reachable,
                uptime_s = obs.uptime_s.unwrap_or(0),
                failed_units = obs.failed_units.len(),
                "healthy"
            ),
            Verdict::Waiting { reason, remaining } => tracing::warn!(
                rule = reason.rule(),
                remaining_s = remaining.as_secs(),
                backoff_s = tracker.backoff().as_secs(),
                "unhealthy ({reason}), but holding off the restart"
            ),
            Verdict::Restart(reason) | Verdict::StartService(reason) => {
                let start_only = matches!(reason, Reason::ServiceStopped(_));
                tracing::warn!(
                    rule = reason.rule(),
                    action = if start_only { "start" } else { "restart" },
                    "unhealthy ({reason}); restarting the recorder"
                );
                // The attempt counts either way: a restart that fails must not
                // be retried on the next 10-second poll.
                tracker.restarted(Instant::now());
                match act(args, start_only) {
                    Ok(()) => tracing::info!(rule = reason.rule(), "recorder restarted"),
                    Err(e) => tracing::error!(error = %e, "restart failed"),
                }
            }
        }

        // Sleep in slices so a stop is honoured within a poll, not an interval.
        let deadline = Instant::now() + args.interval;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200).min(args.interval));
        }
    }

    tracing::info!("trunkd monitor stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn th() -> Thresholds {
        Thresholds {
            unreachable_after: Duration::from_secs(60),
            unlocked_after: Duration::from_secs(600),
            failed_after: Duration::from_secs(30),
            stopped_after: Duration::from_secs(30),
            min_interval: Duration::from_secs(120),
            max_interval: Duration::from_secs(1800),
        }
    }

    fn t0() -> Instant {
        // Far enough in that subtracting the longest window never underflows.
        Instant::now()
    }

    fn secs(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    fn healthy(uptime: u64) -> Observation {
        Observation {
            reachable: true,
            any_locked: true,
            has_control_tuners: true,
            failed_units: vec![],
            uptime_s: Some(uptime),
        }
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        // A bare number is seconds, the unit an operator means by default.
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("5 fortnights").is_err());
    }

    #[test]
    fn an_unreachable_panel_is_unhealthy_only_after_the_window() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let down = Observation::default();

        assert_eq!(
            t.observe(base, &down, ServiceHealth::Unknown),
            Verdict::Healthy,
            "one failed poll is a blip, not a fault"
        );
        assert_eq!(
            t.observe(secs(base, 59), &down, ServiceHealth::Unknown),
            Verdict::Healthy
        );
        assert!(matches!(
            t.observe(secs(base, 60), &down, ServiceHealth::Unknown),
            Verdict::Restart(Reason::Unreachable(_))
        ));
    }

    #[test]
    fn a_recovered_panel_clears_the_unreachable_window() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        t.observe(base, &Observation::default(), ServiceHealth::Unknown);
        t.observe(secs(base, 50), &healthy(100), ServiceHealth::Up);
        // The window restarts from the new failure, not the old one.
        t.observe(
            secs(base, 55),
            &Observation::default(),
            ServiceHealth::Unknown,
        );
        assert_eq!(
            t.observe(
                secs(base, 110),
                &Observation::default(),
                ServiceHealth::Unknown
            ),
            Verdict::Healthy
        );
        assert!(matches!(
            t.observe(
                secs(base, 115),
                &Observation::default(),
                ServiceHealth::Unknown
            ),
            Verdict::Restart(Reason::Unreachable(_))
        ));
    }

    #[test]
    fn a_tuner_that_never_locks_is_unhealthy_after_the_window() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let unlocked = Observation {
            any_locked: false,
            ..healthy(10)
        };
        assert_eq!(
            t.observe(base, &unlocked, ServiceHealth::Up),
            Verdict::Healthy
        );
        assert_eq!(
            t.observe(secs(base, 599), &unlocked, ServiceHealth::Up),
            Verdict::Healthy
        );
        assert!(matches!(
            t.observe(secs(base, 600), &unlocked, ServiceHealth::Up),
            Verdict::Restart(Reason::NoLock(_))
        ));
    }

    #[test]
    fn an_instance_with_no_control_tuner_never_trips_the_lock_rule() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        // A voice-only or iqfile instance has nothing to lock; restarting it
        // forever would be the monitor inventing a fault.
        let obs = Observation {
            any_locked: false,
            has_control_tuners: false,
            ..healthy(10)
        };
        assert_eq!(
            t.observe(secs(base, 5_000), &obs, ServiceHealth::Up),
            Verdict::Healthy
        );
    }

    #[test]
    fn a_failed_unit_is_unhealthy_after_its_own_shorter_window() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let obs = Observation {
            failed_units: vec!["device:airspy-0".to_owned()],
            ..healthy(10)
        };
        assert_eq!(t.observe(base, &obs, ServiceHealth::Up), Verdict::Healthy);
        match t.observe(secs(base, 30), &obs, ServiceHealth::Up) {
            Verdict::Restart(Reason::FailedUnit(name, _)) => {
                assert_eq!(name, "device:airspy-0");
            }
            other => panic!("expected a failed-unit restart, got {other:?}"),
        }
    }

    #[test]
    fn a_service_that_died_is_started_rather_than_restarted() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let down = Observation::default();
        assert_eq!(
            t.observe(base, &down, ServiceHealth::Down),
            Verdict::Healthy
        );
        assert!(matches!(
            t.observe(secs(base, 30), &down, ServiceHealth::Down),
            Verdict::StartService(Reason::ServiceStopped(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn only_a_zero_exit_code_counts_as_a_deliberate_stop() {
        use ServiceHealth::*;
        // `trunkd service stop`, `sc stop`, services.msc: the SCM records 0.
        assert_eq!(
            classify_service(ServiceState::Stopped, ServiceExitCode::Win32(0)),
            StoppedClean
        );
        assert_eq!(
            classify_service(ServiceState::StopPending, ServiceExitCode::Win32(0)),
            StoppedClean
        );
        // 1067 ERROR_PROCESS_ABORTED is what a kill or a crash leaves behind.
        assert_eq!(
            classify_service(ServiceState::Stopped, ServiceExitCode::Win32(1067)),
            Down
        );
        // trunkd's own fatal-error code is a failure, not an operator decision.
        assert_eq!(
            classify_service(ServiceState::Stopped, ServiceExitCode::ServiceSpecific(1)),
            Down
        );
        assert_eq!(
            classify_service(ServiceState::Running, ServiceExitCode::NO_ERROR),
            Up
        );
        assert_eq!(
            classify_service(ServiceState::StartPending, ServiceExitCode::NO_ERROR),
            Up
        );
    }

    #[test]
    fn a_deliberately_stopped_service_is_left_alone_however_long_it_sits() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        // The panel is unreachable and nothing is locked — both *because* the
        // operator stopped it. Neither is a fault to act on, at any age.
        let down = Observation::default();
        for at in [0, 30, 61, 120, 300] {
            assert_eq!(
                t.observe(secs(base, at), &down, ServiceHealth::StoppedClean),
                Verdict::Idle,
                "at t+{at}s"
            );
        }
    }

    #[test]
    fn the_rules_start_from_scratch_when_a_stopped_service_comes_back() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let down = Observation::default();

        // Five minutes stopped on purpose...
        t.observe(base, &down, ServiceHealth::StoppedClean);
        t.observe(secs(base, 300), &down, ServiceHealth::StoppedClean);

        // ...then started again, but not yet serving its panel. The unreachable
        // window starts now, not five minutes ago.
        assert_eq!(
            t.observe(secs(base, 305), &down, ServiceHealth::Up),
            Verdict::Healthy
        );
        assert_eq!(
            t.observe(secs(base, 360), &down, ServiceHealth::Up),
            Verdict::Healthy,
            "55s into the fresh window, not 360s into a stale one"
        );
        // The normal rules do resume once it has really been unreachable.
        assert!(matches!(
            t.observe(secs(base, 366), &down, ServiceHealth::Up),
            Verdict::Restart(Reason::Unreachable(_))
        ));
    }

    #[test]
    fn a_recorder_restart_resets_every_window() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let unlocked = |up| Observation {
            any_locked: false,
            ..healthy(up)
        };
        // Nine minutes of no lock on the old process...
        t.observe(base, &unlocked(3_000), ServiceHealth::Up);
        t.observe(secs(base, 540), &unlocked(3_540), ServiceHealth::Up);
        // ...then uptime goes backwards: a new process, a new window.
        t.observe(secs(base, 545), &unlocked(5), ServiceHealth::Up);
        assert_eq!(
            t.observe(secs(base, 1_100), &unlocked(560), ServiceHealth::Up),
            Verdict::Healthy,
            "the fresh process gets its full unlocked_after window"
        );
        assert!(matches!(
            t.observe(secs(base, 1_146), &unlocked(606), ServiceHealth::Up),
            Verdict::Restart(Reason::NoLock(_))
        ));
    }

    #[test]
    fn the_backoff_doubles_while_restarts_do_not_take_and_is_capped() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let down = Observation::default();

        // First fault: restart at the 60 s unreachable threshold.
        t.observe(base, &down, ServiceHealth::Unknown);
        assert!(matches!(
            t.observe(secs(base, 60), &down, ServiceHealth::Unknown),
            Verdict::Restart(_)
        ));
        t.restarted(secs(base, 60));
        assert_eq!(t.backoff(), Duration::from_secs(120));

        // Still broken a minute later: escalate to 4 minutes and hold off.
        t.observe(secs(base, 120), &down, ServiceHealth::Unknown);
        match t.observe(secs(base, 180), &down, ServiceHealth::Unknown) {
            Verdict::Waiting { remaining, .. } => {
                assert_eq!(t.backoff(), Duration::from_secs(240));
                assert_eq!(remaining, Duration::from_secs(120));
            }
            other => panic!("expected a hold-off, got {other:?}"),
        }
        // Once the 4 minutes are up, it restarts again.
        assert!(matches!(
            t.observe(secs(base, 300), &down, ServiceHealth::Unknown),
            Verdict::Restart(_)
        ));
        t.restarted(secs(base, 300));

        // Keep failing inside the escalate window every time: the doubling
        // stops at max_interval and never runs away.
        let mut t = HealthTracker::new(th());
        let mut clock = 0u64;
        for expected in [240u64, 480, 960, 1_800, 1_800, 1_800] {
            t.restarted(secs(base, clock));
            t.observe(secs(base, clock + 10), &down, ServiceHealth::Unknown);
            let v = t.observe(secs(base, clock + 70), &down, ServiceHealth::Unknown);
            assert!(matches!(v, Verdict::Waiting { .. }), "{v:?}");
            assert_eq!(t.backoff(), Duration::from_secs(expected));
            clock += t.backoff().as_secs() + 1;
        }
    }

    #[test]
    fn a_full_window_of_health_resets_the_backoff() {
        let mut t = HealthTracker::new(th());
        let base = t0();
        let down = Observation::default();

        // A restart that did not take: the backoff escalates once.
        t.restarted(base);
        t.observe(secs(base, 10), &down, ServiceHealth::Unknown);
        assert!(matches!(
            t.observe(secs(base, 70), &down, ServiceHealth::Unknown),
            Verdict::Waiting { .. }
        ));
        assert_eq!(t.backoff(), Duration::from_secs(240), "escalated once");

        // Then thirty unbroken minutes of health.
        t.observe(secs(base, 80), &healthy(80), ServiceHealth::Up);
        t.observe(secs(base, 1_879), &healthy(1_879), ServiceHealth::Up);
        assert_eq!(t.backoff(), Duration::from_secs(240), "not yet");
        t.observe(secs(base, 1_881), &healthy(1_881), ServiceHealth::Up);
        assert_eq!(t.backoff(), Duration::from_secs(120), "back to the floor");
    }

    #[test]
    fn a_panel_body_becomes_an_observation() {
        let body = json!({
            "instance_id": "box-1",
            "uptime_s": 4242,
            "tuners": [
                {"tuner_key": "airspy-0", "role": "control", "locked": true},
                {"tuner_key": "airspy-1", "role": "voice", "locked": false},
            ],
            "supervisor": [
                {"name": "device:airspy-0", "state": "running"},
                {"name": "device:airspy-1", "state": "failed"},
            ],
        });
        let obs = parse_status(&body);
        assert!(obs.reachable);
        assert!(obs.any_locked);
        assert!(obs.has_control_tuners);
        assert_eq!(obs.failed_units, vec!["device:airspy-1".to_owned()]);
        assert_eq!(obs.uptime_s, Some(4242));

        // A voice-only instance follows no control channel.
        let voice_only = json!({
            "uptime_s": 1,
            "tuners": [{"tuner_key": "a", "role": "voice", "locked": false}],
            "supervisor": [],
        });
        let obs = parse_status(&voice_only);
        assert!(!obs.has_control_tuners);
        assert!(obs.failed_units.is_empty());

        // An unrecognisable body degrades to "nothing to report", never to a
        // fault the monitor invents.
        let obs = parse_status(&json!({}));
        assert!(obs.reachable);
        assert!(!obs.has_control_tuners);
        assert_eq!(obs.uptime_s, None);
    }

    /// A tuner keyup switched off idles by design: it never locks, and
    /// restarting the service every `unlocked_after` would not change that.
    /// It is therefore not a control tuner as far as the lock rule is
    /// concerned — while a tuner that reports no `enabled` field at all (an
    /// older panel) still is.
    #[test]
    fn a_tuner_disabled_by_keyup_is_not_a_tuner_that_failed_to_lock() {
        let disabled = json!({
            "tuners": [{"tuner_key": "hackrf0", "role": "auto", "enabled": false, "locked": false}],
        });
        let obs = parse_status(&disabled);
        assert!(
            !obs.has_control_tuners,
            "a disabled tuner cannot fail to lock"
        );
        assert!(!obs.any_locked);

        let enabled = json!({
            "tuners": [{"tuner_key": "hackrf0", "role": "auto", "enabled": true, "locked": false}],
        });
        assert!(parse_status(&enabled).has_control_tuners);

        // No `enabled` key at all: an older panel, and the rule is unchanged.
        let legacy = json!({
            "tuners": [{"tuner_key": "hackrf0", "role": "auto", "locked": false}],
        });
        assert!(parse_status(&legacy).has_control_tuners);
    }

    #[test]
    fn the_installed_monitor_command_line_names_the_recorder() {
        let args = service_arguments(
            "trunkd",
            DEFAULT_URL,
            Path::new(r"C:\ProgramData\trunkd\logs"),
            Path::new(r"C:\ProgramData\trunkd\trunkd.toml"),
        );
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "monitor",
                "--service",
                "trunkd",
                "--url",
                DEFAULT_URL,
                "--log-dir",
                r"C:\ProgramData\trunkd\logs",
                "--config",
                r"C:\ProgramData\trunkd\trunkd.toml",
            ]
        );
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .into()
    }

    #[test]
    fn the_status_poll_carries_the_token_when_there_is_one() {
        let rq = status_request(&agent(), "http://127.0.0.1:8095/", Some("s3cret"));
        assert_eq!(
            rq.uri_ref().map(|u| u.to_string()).as_deref(),
            Some("http://127.0.0.1:8095/api/status")
        );
        let auth = rq
            .headers_ref()
            .and_then(|h| h.get("Authorization"))
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some("Bearer s3cret"));

        let rq = status_request(&agent(), "http://127.0.0.1:8095", None);
        assert!(rq
            .headers_ref()
            .is_some_and(|h| h.get("Authorization").is_none()));
    }

    /// The installed monitor is given the recorder's config, and finds the
    /// panel token there — the first real one — when nothing overrides it.
    #[test]
    fn the_token_comes_from_the_flag_the_environment_or_the_config() {
        let dir = std::env::temp_dir().join(format!(
            "trunkd-monitor-token-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let with = dir.join("with.toml");
        std::fs::write(
            &with,
            "[server]\ntokens = [\"  \", \"panel-tok\", \"second\"]\n",
        )
        .expect("write");
        let without = dir.join("without.toml");
        std::fs::write(&without, "[server]\nenabled = true\n").expect("write");

        assert_eq!(
            resolve_token(None, None, Some(&with)).as_deref(),
            Some("panel-tok")
        );
        assert_eq!(resolve_token(None, None, Some(&without)), None);
        assert_eq!(
            resolve_token(None, None, Some(&dir.join("missing.toml"))),
            None,
            "an unreadable config means no token, not a failure"
        );
        assert_eq!(resolve_token(None, None, None), None);
        // The flag, then the environment, win over the file; blanks do not.
        assert_eq!(
            resolve_token(Some("flag"), Some("env"), Some(&with)).as_deref(),
            Some("flag")
        );
        assert_eq!(
            resolve_token(Some(" "), Some("env"), Some(&with)).as_deref(),
            Some("env")
        );
        assert_eq!(
            resolve_token(None, Some(""), Some(&with)).as_deref(),
            Some("panel-tok")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 401 is the recorder answering: never a restart on its own.
    #[test]
    fn a_refused_poll_is_reachable_and_restarts_nothing() {
        let obs = observation(&Poll::Refused(401));
        assert!(obs.reachable);
        assert!(
            !obs.has_control_tuners,
            "lock health is unknown, not failing"
        );
        assert!(obs.failed_units.is_empty());
        assert!(!observation(&Poll::Failed).reachable);

        let mut t = HealthTracker::new(th());
        let base = t0();
        for s in (0..3600).step_by(10) {
            let v = t.observe(secs(base, s), &obs, ServiceHealth::Up);
            assert_eq!(v, Verdict::Healthy, "at {s}s");
        }
    }

    /// Against a real socket: the poll sends the token, and a panel that
    /// refuses it is `Refused`, not `Failed`.
    #[test]
    fn a_poll_sends_the_token_and_reads_a_refusal_as_a_refusal() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            s.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).expect("read") == 0 {
                    break;
                }
                head.push(b[0]);
            }
            s.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .expect("write");
            String::from_utf8_lossy(&head).into_owned()
        });
        let answer = poll(&agent(), &url, Some("s3cret"));
        let head = server.join().expect("the fake panel");
        assert!(
            head.lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: Bearer s3cret")),
            "{head}"
        );
        assert_eq!(answer, Poll::Refused(401));
    }
}
