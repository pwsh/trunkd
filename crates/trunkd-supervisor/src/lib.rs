// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-supervisor` — a generic, in-process supervision tree on OS threads.
//!
//! Every long-lived component (device reader, DSP pipeline, site decoder,
//! uplink) runs as a [`Supervised`] unit. The [`Supervisor`] spawns each on its
//! own OS thread, catches panics and error returns, and restarts with
//! exponential backoff up to a cap; too many restarts inside a window escalates
//! (ARCHITECTURE §6). Escalation either gives up ([`AfterBudget::GiveUp`]) or,
//! for a unit that must never stay down, keeps retrying on a slower backoff
//! ([`AfterBudget::RetrySlowly`]) until it runs again or a [`RearmProbe`] says
//! something changed that makes an immediate retry worthwhile. A heartbeat
//! watchdog marks a unit [`ComponentState::Degraded`] when it stops bumping its
//! heartbeat.
//!
//! No async runtime is involved — this is deliberately plain threads.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use trunkd_core::Timestamp;

/// Lifecycle state of a supervised component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentState {
    /// Actively running and (if watched) beating its heartbeat.
    Running,
    /// Running but its heartbeat has gone stale.
    Degraded,
    /// Currently between a failure and its next start.
    Restarting,
    /// Spent its restart budget but still being retried, on the slower
    /// [`AfterBudget::RetrySlowly`] backoff: down, not given up on.
    Retrying,
    /// Permanently stopped: exceeded the restart budget, or completed.
    Failed,
}

/// What a unit does once it has spent its restart budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterBudget {
    /// Stop for good ([`ComponentState::Failed`]) and fire the escalation hook.
    GiveUp,
    /// Fire the escalation hook, then keep retrying ([`ComponentState::Retrying`])
    /// with an exponential backoff from `base` up to `max`. A run that lasts a
    /// whole `window`, or a [`RearmProbe`] that moves, puts the unit back on its
    /// ordinary budget.
    RetrySlowly {
        /// First delay once the budget is spent.
        base: Duration,
        /// Ceiling on the delay.
        max: Duration,
    },
}

/// Error returned by a unit's `run`. Any value means "failed, restart me".
pub type RunError = String;

/// A supervised unit of work that runs until it returns or is asked to stop.
pub trait Supervised: Send {
    /// Execute the unit. Return `Ok(())` for a clean, intentional completion
    /// (no restart), or `Err` to signal failure (restart per policy). Panicking
    /// is treated the same as returning `Err`.
    ///
    /// Implementors should periodically call [`SupervisionContext::heartbeat`]
    /// and stop promptly when [`SupervisionContext::should_stop`] is true.
    fn run(&mut self, ctx: &SupervisionContext) -> Result<(), RunError>;
}

/// Restart/backoff policy for one supervised unit.
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// First backoff after a failure.
    pub base_backoff: Duration,
    /// Upper bound on backoff.
    pub max_backoff: Duration,
    /// Multiplier applied per consecutive failure.
    pub factor: u32,
    /// Failures allowed within `window` before escalation.
    pub max_restarts: u32,
    /// Sliding window over which `max_restarts` is counted.
    pub window: Duration,
    /// If set, the watchdog marks the unit Degraded when its heartbeat is older
    /// than this.
    pub heartbeat_timeout: Option<Duration>,
    /// What happens once `max_restarts` failures land inside `window`.
    pub after_budget: AfterBudget,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy {
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
            factor: 2,
            max_restarts: 5,
            window: Duration::from_secs(60),
            heartbeat_timeout: Some(Duration::from_secs(10)),
            after_budget: AfterBudget::GiveUp,
        }
    }
}

/// Shared, observable state for one supervised unit.
#[derive(Debug)]
struct UnitShared {
    name: String,
    state: Mutex<ComponentState>,
    restart_count: AtomicU64,
    heartbeat_ms: AtomicU64,
    shutdown: AtomicBool,
    /// Set by the unit itself to ask for a clean rebuild that is *not* a
    /// failure: the config-change path (a keyup desired-state apply restarts a
    /// `DeviceUnit` with new params). Distinct from `shutdown` (terminal) and
    /// from returning `Err` (a fault, which counts against the restart budget).
    restart_requested: AtomicBool,
    heartbeat_timeout_ms: u64,
    /// Unix ms of the next scheduled attempt while waiting out a backoff; 0
    /// while the unit is running (or has stopped).
    next_attempt_ms: AtomicU64,
    /// The last failure's error (or "panicked"), kept for diagnosis until a
    /// run ends in a clean requested restart.
    last_error: Mutex<Option<String>>,
}

impl UnitShared {
    fn set_state(&self, s: ComponentState) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = s;
        }
    }

    fn state(&self) -> ComponentState {
        self.state
            .lock()
            .map(|g| *g)
            .unwrap_or(ComponentState::Failed)
    }

    fn beat(&self) {
        self.heartbeat_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn set_last_error(&self, e: Option<String>) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = e;
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn next_attempt_ms(&self) -> Option<u64> {
        match self.next_attempt_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }
}

/// Passed to [`Supervised::run`]; the unit uses it to beat and to observe
/// shutdown.
pub struct SupervisionContext {
    shared: Arc<UnitShared>,
}

impl SupervisionContext {
    /// Record a heartbeat now. Call this regularly from the unit's loop.
    pub fn heartbeat(&self) {
        self.shared.beat();
    }

    /// Whether the supervisor has asked this unit to stop.
    pub fn should_stop(&self) -> bool {
        self.shared.shutdown.load(Ordering::Relaxed)
    }

    /// Ask the supervisor to rebuild this unit after it returns, without
    /// counting the return as a failure and without backoff.
    ///
    /// This is how a live config change is applied: the unit observes the new
    /// desired state, calls this, and returns `Ok(())`; the supervisor's next
    /// `factory()` call builds the unit with the new parameters. Unlike an
    /// `Err` return, it does not consume the restart budget or log a fault.
    pub fn request_restart(&self) {
        self.shared.restart_requested.store(true, Ordering::Relaxed);
    }
}

/// A read-only, cloneable view of one supervised unit's health.
///
/// [`SupervisedHandle::stop`] consumes the handle, so a handle cannot be shared
/// with an observer that outlives teardown. This view holds only the unit's
/// shared cell, so a status reader (the local control panel) can watch a unit
/// for as long as it likes without owning — or blocking — its lifecycle.
#[derive(Clone, Debug)]
pub struct UnitStatus {
    shared: Arc<UnitShared>,
}

impl UnitStatus {
    /// The unit's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ComponentState {
        self.shared.state()
    }

    /// Number of restarts so far.
    pub fn restart_count(&self) -> u64 {
        self.shared.restart_count.load(Ordering::Relaxed)
    }

    /// Milliseconds since the last heartbeat.
    pub fn heartbeat_age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.shared.heartbeat_ms.load(Ordering::Relaxed))
    }

    /// When the next attempt is due (unix ms) while the unit waits out a
    /// backoff; `None` while it is running.
    pub fn next_attempt_ms(&self) -> Option<u64> {
        self.shared.next_attempt_ms()
    }

    /// The last failure's error, until the unit has run healthily again.
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error()
    }
}

/// Handle to a spawned supervised unit.
pub struct SupervisedHandle {
    shared: Arc<UnitShared>,
    monitor: Option<JoinHandle<()>>,
}

impl SupervisedHandle {
    /// The unit's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ComponentState {
        self.shared.state()
    }

    /// Number of restarts so far.
    pub fn restart_count(&self) -> u64 {
        self.shared.restart_count.load(Ordering::Relaxed)
    }

    /// Milliseconds since the last heartbeat.
    pub fn heartbeat_age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.shared.heartbeat_ms.load(Ordering::Relaxed))
    }

    /// A read-only view of this unit's health, cloneable and outliving the
    /// handle itself.
    pub fn status(&self) -> UnitStatus {
        UnitStatus {
            shared: self.shared.clone(),
        }
    }

    /// Ask the unit to stop and wait for its monitor thread to finish.
    pub fn stop(mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.monitor.take() {
            let _ = h.join();
        }
    }
}

/// Factory that produces a fresh unit for each (re)start.
pub type UnitFactory = Box<dyn Fn() -> Box<dyn Supervised> + Send + Sync>;

/// Callback invoked when a unit exhausts its restart budget: once for
/// [`AfterBudget::GiveUp`], and each time it moves onto the slow backoff for
/// [`AfterBudget::RetrySlowly`].
pub type EscalationHook = Box<dyn Fn(&str) + Send + Sync>;

/// One failure, as reported to a [`FailureHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureReport<'a> {
    /// The unit's name.
    pub name: &'a str,
    /// The error the unit returned, or `"panicked"`.
    pub error: &'a str,
    /// Consecutive failures since the unit last ran healthily or was re-armed.
    pub failures: u32,
    /// Whether the unit has spent its budget and is on the slow backoff.
    pub retrying: bool,
    /// When the next attempt is due (unix ms); `None` when there will be none
    /// (the unit gave up).
    pub next_attempt_ms: Option<u64>,
}

/// Callback invoked after every failure, before the backoff sleep.
pub type FailureHook = Box<dyn Fn(&FailureReport<'_>) + Send + Sync>;

/// A counter the supervisor samples while a unit waits out a backoff. When it
/// moves (someone asked for a restart, or the unit's configuration changed),
/// the wait is cut short and the unit's backoff and budget start over.
pub type RearmProbe = Box<dyn Fn() -> u64 + Send + Sync>;

/// Optional callbacks for one supervised unit.
#[derive(Default)]
pub struct UnitHooks {
    /// See [`EscalationHook`].
    pub on_escalate: Option<EscalationHook>,
    /// See [`FailureHook`].
    pub on_failure: Option<FailureHook>,
    /// See [`RearmProbe`].
    pub rearm: Option<RearmProbe>,
}

/// Spawns and supervises units, and runs a heartbeat watchdog over them.
#[derive(Default)]
pub struct Supervisor {
    units: Arc<Mutex<Vec<Arc<UnitShared>>>>,
    watchdog: Option<JoinHandle<()>>,
    watchdog_stop: Arc<AtomicBool>,
}

impl Supervisor {
    /// Create an empty supervisor.
    pub fn new() -> Self {
        Supervisor::default()
    }

    /// Spawn `factory` under supervision with `policy`. `on_escalate` fires
    /// if the unit exceeds its restart budget.
    pub fn spawn(
        &mut self,
        name: impl Into<String>,
        factory: UnitFactory,
        policy: RestartPolicy,
        on_escalate: Option<EscalationHook>,
    ) -> SupervisedHandle {
        self.spawn_with(
            name,
            factory,
            policy,
            UnitHooks {
                on_escalate,
                ..UnitHooks::default()
            },
        )
    }

    /// [`Supervisor::spawn`] with the full set of [`UnitHooks`].
    pub fn spawn_with(
        &mut self,
        name: impl Into<String>,
        factory: UnitFactory,
        policy: RestartPolicy,
        hooks: UnitHooks,
    ) -> SupervisedHandle {
        let shared = Arc::new(UnitShared {
            name: name.into(),
            state: Mutex::new(ComponentState::Restarting),
            restart_count: AtomicU64::new(0),
            heartbeat_ms: AtomicU64::new(now_ms()),
            shutdown: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            heartbeat_timeout_ms: policy
                .heartbeat_timeout
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            next_attempt_ms: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });
        if let Ok(mut units) = self.units.lock() {
            units.push(shared.clone());
        }

        let monitor_shared = shared.clone();
        let monitor = thread::Builder::new()
            .name(format!("supervise:{}", shared.name))
            .spawn(move || monitor_loop(monitor_shared, factory, policy, hooks))
            .ok();

        self.ensure_watchdog();

        SupervisedHandle { shared, monitor }
    }

    fn ensure_watchdog(&mut self) {
        if self.watchdog.is_some() {
            return;
        }
        // The watchdog scans the shared unit list, so units spawned after it
        // starts are watched too.
        let units = self.units.clone();
        let stop = self.watchdog_stop.clone();
        self.watchdog = thread::Builder::new()
            .name("supervise:watchdog".to_owned())
            .spawn(move || watchdog_loop(units, stop))
            .ok();
    }

    /// Stop the watchdog thread. Individual units are stopped via their handles.
    pub fn shutdown(&mut self) {
        self.watchdog_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn monitor_loop(
    shared: Arc<UnitShared>,
    factory: UnitFactory,
    policy: RestartPolicy,
    hooks: UnitHooks,
) {
    let mut failures_in_window: Vec<Instant> = Vec::new();
    let mut consecutive: u32 = 0;
    // On the slow backoff: the budget is spent and the unit is being retried
    // regardless (only with `AfterBudget::RetrySlowly`).
    let mut retrying = false;
    let mut slow_consecutive: u32 = 0;
    let probe = || hooks.rearm.as_ref().map_or(0, |p| p());

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            shared.next_attempt_ms.store(0, Ordering::Relaxed);
            shared.set_state(ComponentState::Failed);
            return;
        }

        shared.next_attempt_ms.store(0, Ordering::Relaxed);
        shared.set_state(ComponentState::Running);
        shared.beat();

        // What the re-arm counter read when this attempt started: a change
        // from here on is news the failure below has not already seen.
        let armed_at = probe();
        let mut unit = factory();
        let ctx = SupervisionContext {
            shared: shared.clone(),
        };
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| unit.run(&ctx)));
        let ran_for = started.elapsed();

        // A restart the unit asked for (a live config change) is not a failure:
        // rebuild immediately, no backoff, no budget spent. Checked before the
        // clean-completion return because the unit returns `Ok(())` to yield.
        //
        // Gated on a clean `Ok(())`: the flag is always cleared, but a unit that
        // requested a restart and *then* faulted (Err or panic) falls through to
        // normal failure accounting, so a buggy unit cannot loop with zero
        // backoff and no budget (a masked restart storm).
        let restart_requested = shared.restart_requested.swap(false, Ordering::Relaxed);
        if restart_requested
            && matches!(outcome, Ok(Ok(())))
            && !shared.shutdown.load(Ordering::Relaxed)
        {
            tracing::info!(unit = %shared.name, "restarting unit to apply a config change");
            consecutive = 0;
            retrying = false;
            slow_consecutive = 0;
            shared.set_last_error(None);
            shared.set_state(ComponentState::Restarting);
            continue;
        }

        // Clean, intentional completion → done, no restart.
        if let Ok(Ok(())) = &outcome {
            shared.set_state(ComponentState::Failed);
            return;
        }
        if shared.shutdown.load(Ordering::Relaxed) {
            shared.set_state(ComponentState::Failed);
            return;
        }

        let error = match &outcome {
            Ok(Err(e)) => {
                tracing::warn!(unit = %shared.name, error = %e, "unit failed");
                e.clone()
            }
            Err(_) => {
                tracing::error!(unit = %shared.name, "unit panicked");
                "panicked".to_owned()
            }
            Ok(Ok(())) => unreachable!(),
        };
        shared.set_last_error(Some(error.clone()));

        shared.restart_count.fetch_add(1, Ordering::Relaxed);

        // A run that lasted longer than the window is treated as healthy: reset
        // the consecutive-failure backoff counter, and a unit that was on the
        // slow backoff is back on its ordinary budget.
        if ran_for >= policy.window {
            consecutive = 0;
            retrying = false;
            slow_consecutive = 0;
        }

        let now = Instant::now();
        failures_in_window.push(now);
        failures_in_window.retain(|t| now.duration_since(*t) <= policy.window);

        if !retrying && failures_in_window.len() as u32 > policy.max_restarts {
            tracing::error!(
                unit = %shared.name,
                restarts = failures_in_window.len(),
                "restart budget exhausted; escalating"
            );
            match policy.after_budget {
                AfterBudget::GiveUp => {
                    shared.set_state(ComponentState::Failed);
                    if let Some(hook) = &hooks.on_failure {
                        hook(&FailureReport {
                            name: &shared.name,
                            error: &error,
                            failures: consecutive.saturating_add(1),
                            retrying: false,
                            next_attempt_ms: None,
                        });
                    }
                    if let Some(hook) = &hooks.on_escalate {
                        hook(&shared.name);
                    }
                    return;
                }
                AfterBudget::RetrySlowly { .. } => {
                    retrying = true;
                    slow_consecutive = 0;
                    if let Some(hook) = &hooks.on_escalate {
                        hook(&shared.name);
                    }
                }
            }
        }

        let backoff = match (retrying, policy.after_budget) {
            (true, AfterBudget::RetrySlowly { base, max }) => {
                let d = backoff_between(base, max, policy.factor, slow_consecutive);
                slow_consecutive = slow_consecutive.saturating_add(1);
                d
            }
            _ => compute_backoff(&policy, consecutive),
        };
        consecutive = consecutive.saturating_add(1);
        let next_at = now_ms().saturating_add(backoff.as_millis() as u64);
        shared.next_attempt_ms.store(next_at, Ordering::Relaxed);
        shared.set_state(if retrying {
            ComponentState::Retrying
        } else {
            ComponentState::Restarting
        });
        if let Some(hook) = &hooks.on_failure {
            hook(&FailureReport {
                name: &shared.name,
                error: &error,
                failures: consecutive,
                retrying,
                next_attempt_ms: Some(next_at),
            });
        }

        let rearmed = match &hooks.rearm {
            Some(p) => sleep_unless_rearmed(backoff, &shared.shutdown, p, armed_at),
            None => {
                interruptible_sleep(backoff, &shared.shutdown);
                false
            }
        };
        if rearmed {
            tracing::info!(
                unit = %shared.name,
                "re-armed (restart requested or configuration changed); retrying now"
            );
            failures_in_window.clear();
            consecutive = 0;
            retrying = false;
            slow_consecutive = 0;
        }
    }
}

fn compute_backoff(policy: &RestartPolicy, consecutive: u32) -> Duration {
    backoff_between(
        policy.base_backoff,
        policy.max_backoff,
        policy.factor,
        consecutive,
    )
}

fn backoff_between(base: Duration, max: Duration, factor: u32, consecutive: u32) -> Duration {
    let base = base.as_millis() as u64;
    let factor = factor.max(1) as u64;
    let mult = factor.checked_pow(consecutive).unwrap_or(u64::MAX);
    let ms = base.saturating_mul(mult);
    let capped = ms.min(max.as_millis() as u64);
    Duration::from_millis(capped)
}

/// Sleep `dur`, or less: returns early (false) on shutdown, and early with
/// `true` as soon as `probe` reads something other than `armed_at`.
fn sleep_unless_rearmed(
    dur: Duration,
    stop: &AtomicBool,
    probe: &RearmProbe,
    armed_at: u64,
) -> bool {
    let step = Duration::from_millis(20);
    let deadline = Instant::now() + dur;
    loop {
        if probe() != armed_at {
            return true;
        }
        let now = Instant::now();
        if stop.load(Ordering::Relaxed) || now >= deadline {
            return false;
        }
        thread::sleep(step.min(deadline - now));
    }
}

fn watchdog_loop(units: Arc<Mutex<Vec<Arc<UnitShared>>>>, stop: Arc<AtomicBool>) {
    let tick = Duration::from_millis(200);
    while !stop.load(Ordering::Relaxed) {
        let snapshot: Vec<Arc<UnitShared>> = match units.lock() {
            Ok(g) => g.clone(),
            Err(_) => Vec::new(),
        };
        for u in &snapshot {
            if u.heartbeat_timeout_ms == 0 {
                continue;
            }
            if u.state() != ComponentState::Running {
                continue;
            }
            let age = now_ms().saturating_sub(u.heartbeat_ms.load(Ordering::Relaxed));
            if age > u.heartbeat_timeout_ms {
                tracing::warn!(unit = %u.name, age_ms = age, "heartbeat stale; marking Degraded");
                u.set_state(ComponentState::Degraded);
            }
        }
        interruptible_sleep(tick, &stop);
    }
}

fn interruptible_sleep(dur: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(20);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let nap = remaining.min(step);
        thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
}

fn now_ms() -> u64 {
    Timestamp::now().unix_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A unit that bumps a counter then panics, to exercise restart/backoff.
    struct PanicUnit {
        starts: Arc<AtomicU32>,
    }

    impl Supervised for PanicUnit {
        fn run(&mut self, _ctx: &SupervisionContext) -> Result<(), RunError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            panic!("boom");
        }
    }

    #[test]
    fn panicking_unit_is_restarted_with_backoff() {
        let starts = Arc::new(AtomicU32::new(0));
        let s2 = starts.clone();
        let mut sup = Supervisor::new();
        let policy = RestartPolicy {
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(80),
            factor: 2,
            max_restarts: 1000, // don't escalate during this test
            window: Duration::from_secs(60),
            heartbeat_timeout: None,
            after_budget: AfterBudget::GiveUp,
        };
        let handle = sup.spawn(
            "panic-unit",
            Box::new(move || Box::new(PanicUnit { starts: s2.clone() })),
            policy,
            None,
        );

        // Give it time to accumulate several backed-off restarts.
        thread::sleep(Duration::from_millis(400));
        let count = starts.load(Ordering::Relaxed);
        assert!(count >= 3, "expected multiple restarts, got {count}");
        assert!(handle.restart_count() >= 3);
        handle.stop();
    }

    /// A unit that panics immediately, to exercise escalation.
    struct AlwaysPanic;
    impl Supervised for AlwaysPanic {
        fn run(&mut self, _ctx: &SupervisionContext) -> Result<(), RunError> {
            panic!("always");
        }
    }

    #[test]
    fn exceeding_max_restarts_escalates() {
        let escalated = Arc::new(AtomicBool::new(false));
        let e2 = escalated.clone();
        let mut sup = Supervisor::new();
        let policy = RestartPolicy {
            base_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            factor: 2,
            max_restarts: 3,
            window: Duration::from_secs(60),
            heartbeat_timeout: None,
            after_budget: AfterBudget::GiveUp,
        };
        let handle = sup.spawn(
            "always-panic",
            Box::new(|| Box::new(AlwaysPanic)),
            policy,
            Some(Box::new(move |_name| {
                e2.store(true, Ordering::Relaxed);
            })),
        );

        // 4th failure (> max_restarts=3) escalates.
        let mut waited = 0;
        while !escalated.load(Ordering::Relaxed) && waited < 3000 {
            thread::sleep(Duration::from_millis(20));
            waited += 20;
        }
        assert!(escalated.load(Ordering::Relaxed), "should have escalated");
        assert_eq!(handle.state(), ComponentState::Failed);
        handle.stop();
    }

    /// A heartbeat-less unit that just spins until told to stop.
    struct SilentUnit;
    impl Supervised for SilentUnit {
        fn run(&mut self, ctx: &SupervisionContext) -> Result<(), RunError> {
            // Never beats after the initial one set by the monitor.
            while !ctx.should_stop() {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        }
    }

    /// A unit that asks for a restart on its first run, then idles.
    struct RestartOnce {
        runs: Arc<AtomicU32>,
    }
    impl Supervised for RestartOnce {
        fn run(&mut self, ctx: &SupervisionContext) -> Result<(), RunError> {
            let n = self.runs.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                ctx.request_restart();
                return Ok(());
            }
            while !ctx.should_stop() {
                ctx.heartbeat();
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    }

    #[test]
    fn requested_restart_rebuilds_without_spending_the_budget() {
        let runs = Arc::new(AtomicU32::new(0));
        let r2 = runs.clone();
        let escalated = Arc::new(AtomicBool::new(false));
        let e2 = escalated.clone();
        let mut sup = Supervisor::new();
        let policy = RestartPolicy {
            // A budget of 1 would escalate immediately if the requested restart
            // were counted as a failure; it must not be.
            max_restarts: 1,
            base_backoff: Duration::from_millis(5),
            heartbeat_timeout: None,
            ..RestartPolicy::default()
        };
        let handle = sup.spawn(
            "restart-once",
            Box::new(move || Box::new(RestartOnce { runs: r2.clone() })),
            policy,
            Some(Box::new(move |_| e2.store(true, Ordering::Relaxed))),
        );
        // Give it time to run twice (the requested restart, then the idle run).
        let mut waited = 0;
        while runs.load(Ordering::Relaxed) < 2 && waited < 2000 {
            thread::sleep(Duration::from_millis(10));
            waited += 10;
        }
        assert!(runs.load(Ordering::Relaxed) >= 2, "it should have rebuilt");
        assert!(!escalated.load(Ordering::Relaxed), "must not escalate");
        assert_eq!(handle.state(), ComponentState::Running);
        handle.stop();
    }

    /// A unit that requests a restart and then faults — the masked-storm case.
    struct RestartThenFail;
    impl Supervised for RestartThenFail {
        fn run(&mut self, ctx: &SupervisionContext) -> Result<(), RunError> {
            ctx.request_restart();
            Err("faulted after requesting a restart".to_owned())
        }
    }

    #[test]
    fn a_requested_restart_that_then_faults_still_spends_the_budget() {
        // If the Err were masked by the restart request, this would loop forever
        // with no backoff and never escalate. It must escalate.
        let escalated = Arc::new(AtomicBool::new(false));
        let e2 = escalated.clone();
        let mut sup = Supervisor::new();
        let policy = RestartPolicy {
            base_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            factor: 2,
            max_restarts: 3,
            window: Duration::from_secs(60),
            heartbeat_timeout: None,
            after_budget: AfterBudget::GiveUp,
        };
        let handle = sup.spawn(
            "restart-then-fail",
            Box::new(|| Box::new(RestartThenFail)),
            policy,
            Some(Box::new(move |_| e2.store(true, Ordering::Relaxed))),
        );
        let mut waited = 0;
        while !escalated.load(Ordering::Relaxed) && waited < 3000 {
            thread::sleep(Duration::from_millis(20));
            waited += 20;
        }
        assert!(
            escalated.load(Ordering::Relaxed),
            "a fault after a restart request must count against the budget"
        );
        assert_eq!(handle.state(), ComponentState::Failed);
        handle.stop();
    }

    /// A unit that always fails the way a missing tuner does: at once, with an
    /// error, counting its starts.
    struct AlwaysErr {
        starts: Arc<AtomicU32>,
    }
    impl Supervised for AlwaysErr {
        fn run(&mut self, _ctx: &SupervisionContext) -> Result<(), RunError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Err("device not found".to_owned())
        }
    }

    fn retry_policy(slow_base: Duration, slow_max: Duration) -> RestartPolicy {
        RestartPolicy {
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            factor: 2,
            max_restarts: 2,
            window: Duration::from_secs(60),
            heartbeat_timeout: None,
            after_budget: AfterBudget::RetrySlowly {
                base: slow_base,
                max: slow_max,
            },
        }
    }

    /// `(retrying, failures, delay to the next attempt in ms)` per failure.
    type Reports = Arc<Mutex<Vec<(bool, u32, u64)>>>;

    fn recording_hooks(reports: &Reports, escalations: &Arc<AtomicU32>) -> UnitHooks {
        let (r, e) = (reports.clone(), escalations.clone());
        UnitHooks {
            on_escalate: Some(Box::new(move |_| {
                e.fetch_add(1, Ordering::Relaxed);
            })),
            on_failure: Some(Box::new(move |f| {
                let delay = f
                    .next_attempt_ms
                    .map_or(0, |at| at.saturating_sub(now_ms()));
                r.lock().unwrap().push((f.retrying, f.failures, delay));
            })),
            rearm: None,
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn a_unit_past_its_budget_keeps_retrying_with_a_growing_delay() {
        let starts = Arc::new(AtomicU32::new(0));
        let s2 = starts.clone();
        let reports: Reports = Arc::default();
        let escalations = Arc::new(AtomicU32::new(0));
        let mut sup = Supervisor::new();
        let handle = sup.spawn_with(
            "flaky",
            Box::new(move || Box::new(AlwaysErr { starts: s2.clone() })),
            retry_policy(Duration::from_millis(20), Duration::from_millis(160)),
            recording_hooks(&reports, &escalations),
        );
        // 3 fast failures spend the budget; then 20, 40, 80, 160, 160 ms.
        assert!(
            wait_until(
                || starts.load(Ordering::Relaxed) >= 8,
                Duration::from_secs(5)
            ),
            "it must keep being retried past its budget"
        );
        assert_ne!(handle.state(), ComponentState::Failed, "never given up on");
        assert_eq!(escalations.load(Ordering::Relaxed), 1, "escalated once");
        assert_eq!(
            handle.status().last_error().as_deref(),
            Some("device not found")
        );

        let reports = reports.lock().unwrap().clone();
        let slow: Vec<u64> = reports
            .iter()
            .filter(|(retrying, _, _)| *retrying)
            .map(|(_, _, d)| *d)
            .collect();
        assert!(slow.len() >= 4, "{reports:?}");
        assert!(
            slow.windows(2).all(|w| w[1] + 2 >= w[0]),
            "never shrinks: {slow:?}"
        );
        assert!(slow[3] >= 3 * slow[0], "grows: {slow:?}");
        assert!(slow.iter().all(|d| *d <= 161), "capped: {slow:?}");
        // Failures keep counting up across the switch to the slow backoff.
        assert!(
            reports.windows(2).all(|w| w[1].1 == w[0].1 + 1),
            "{reports:?}"
        );
        handle.stop();
    }

    #[test]
    fn moving_the_rearm_probe_retries_at_once_and_resets_the_budget() {
        let starts = Arc::new(AtomicU32::new(0));
        let s2 = starts.clone();
        let reports: Reports = Arc::default();
        let escalations = Arc::new(AtomicU32::new(0));
        let generation = Arc::new(AtomicU64::new(0));
        let g2 = generation.clone();
        let mut hooks = recording_hooks(&reports, &escalations);
        hooks.rearm = Some(Box::new(move || g2.load(Ordering::Relaxed)));
        let mut sup = Supervisor::new();
        // A slow backoff far longer than the test: only a re-arm can bring
        // the unit back inside it.
        let handle = sup.spawn_with(
            "rearmed",
            Box::new(move || Box::new(AlwaysErr { starts: s2.clone() })),
            retry_policy(Duration::from_secs(600), Duration::from_secs(600)),
            hooks,
        );
        assert!(wait_until(
            || handle.state() == ComponentState::Retrying,
            Duration::from_secs(5)
        ));
        let before = starts.load(Ordering::Relaxed);
        let next = handle
            .status()
            .next_attempt_ms()
            .expect("a scheduled retry");
        assert!(next > now_ms() + 500_000, "ten minutes out");

        generation.fetch_add(1, Ordering::Relaxed);
        assert!(
            wait_until(
                || starts.load(Ordering::Relaxed) > before,
                Duration::from_secs(2)
            ),
            "a re-arm must retry at once, not in ten minutes"
        );
        // Back on the ordinary budget: the failures after the re-arm count
        // from one again and are not on the slow backoff until it is spent.
        assert!(wait_until(
            || handle.state() == ComponentState::Retrying,
            Duration::from_secs(5)
        ));
        let reports = reports.lock().unwrap().clone();
        let after: Vec<_> = reports.iter().skip(before as usize).collect();
        assert!(!after[0].0, "{reports:?}");
        assert_eq!(after[0].1, 1, "{reports:?}");
        assert_eq!(escalations.load(Ordering::Relaxed), 2, "budget spent again");
        handle.stop();
    }

    #[test]
    fn stale_heartbeat_marks_degraded() {
        let mut sup = Supervisor::new();
        let policy = RestartPolicy {
            heartbeat_timeout: Some(Duration::from_millis(100)),
            ..RestartPolicy::default()
        };
        let handle = sup.spawn("silent", Box::new(|| Box::new(SilentUnit)), policy, None);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(handle.state(), ComponentState::Degraded);
        handle.stop();
    }
}
