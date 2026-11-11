// SPDX-License-Identifier: GPL-3.0-or-later
//! Native Windows service integration: register, control, and be hosted by the
//! Service Control Manager.
//!
//! Windows has no systemd, and "log in, open a terminal, run `trunkd run`" is
//! not a deployment: an unattended recorder has to survive a reboot and a
//! logout. This module is the whole Windows answer to that — the `service`
//! subcommand family (install/uninstall/start/stop/status) plus the SCM-facing
//! entry point (`service run`) the SCM itself launches.
//!
//! On Linux the supported path is a **systemd unit** (see `docs/install-linux.md`);
//! nothing here is built there, and the `service` subcommand says so.
//!
//! # Shape
//!
//! Everything that talks to the SCM is deliberately thin and untestable-by-hand
//! (`install`, the control wrappers, [`host_or_return`]); everything that makes
//! a *decision* — the config path, the launch command line, the log directory —
//! is a small pure function above it with unit tests, because those are the
//! parts that go wrong silently.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use trunkd_core::{Error, Result};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

/// Default service name (the SCM key, and what `sc.exe` wants).
pub const DEFAULT_NAME: &str = "trunkd";
/// Suffix appended to the recorder's name for the optional health monitor.
pub const MONITOR_SUFFIX: &str = "-monitor";
/// Display name of the optional health monitor service.
pub const MONITOR_DISPLAY_NAME: &str = "trunkd health monitor";

const DESCRIPTION: &str = "P25 trunked-radio recorder (trunkd)";
const MONITOR_DESCRIPTION: &str =
    "Watches the trunkd recorder's local panel and restarts it when it stops recording";

/// How long a clean shutdown may take before the SCM decides we are hung.
///
/// The recorder finishes its open calls and drains the uplink on the way out,
/// which is seconds of work, not milliseconds; 30 s is generous enough that a
/// busy site never gets killed mid-call.
const STOP_WAIT_HINT: Duration = Duration::from_secs(30);

/// How long `service uninstall` waits for a running service to stop.
const UNINSTALL_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll granularity while waiting on an SCM state transition.
const POLL: Duration = Duration::from_millis(250);

/// Exit code reported to the SCM when the recorder returns an error.
///
/// Service-specific rather than a Win32 code: the SCM's failure actions fire on
/// any non-zero exit, and a made-up Win32 number would be decoded into an
/// unrelated system error message in the event log.
const EXIT_CODE_FAILED: u32 = 1;

// ---------------------------------------------------------------------------
// Pure helpers — the decisions, testable without an SCM.
// ---------------------------------------------------------------------------

/// Resolve the `--config` path a service will be launched with.
///
/// A service starts in `C:\Windows\System32` with no inherited environment, so
/// a relative path recorded at install time would mean something different (or
/// nothing) at start time. Refuse anything that does not exist *now* rather
/// than register a service that will fail its first start with a bare
/// "file not found" in the event log.
pub fn resolve_config_path(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        return Err(Error::config(format!(
            "config file not found: {} — `trunkd service install` records an \
             absolute path, so the file has to exist now",
            path.display()
        )));
    }
    let full = path.canonicalize()?;
    Ok(strip_verbatim(&full))
}

/// Drop Windows' `\\?\` verbatim prefix from a canonicalised path.
///
/// `canonicalize` always returns the verbatim form. The SCM accepts it, but it
/// then shows up verbatim in `sc qc`, in the event log, and in every error
/// message an operator reads, which is noise. Only the plain drive form is
/// stripped: a UNC verbatim path (`\\?\UNC\...`) means something else and is
/// left alone.
fn strip_verbatim(path: &Path) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Where a service writes its rolling log, given the config path and whatever
/// `[server] log_dir` says.
///
/// Both installed services go through this so they agree: an operator who set
/// `[server] log_dir` expects the recorder *and* its monitor to be in that one
/// directory, not scattered between it and the config folder. A relative
/// `log_dir` is resolved against the **config's** directory, which is where the
/// service sets its working directory, so it means the same thing as it does
/// when trunkd is run by hand from there. With nothing configured it is
/// `<config dir>\logs`: the config directory is the one the operator already
/// owns and already looks in.
pub fn service_log_dir(config: &Path, configured: Option<&str>) -> PathBuf {
    let base = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // `Path::join` returns the argument untouched when it is already absolute,
    // which is exactly the rule wanted here.
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => base.join(dir),
        None => base.join("logs"),
    }
}

/// The arguments the SCM launches the recorder service with.
pub fn recorder_arguments(config: &Path) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--config"),
        config.as_os_str().to_os_string(),
    ]
}

/// Quote one command-line argument the way Windows' `CommandLineToArgvW`
/// unquotes it.
///
/// Only used to *display* and log the command line: `windows-service` does the
/// real escaping when it builds `lpBinaryPathName`. Same rule, so the two agree
/// — the whole point of showing it is that an operator can paste it into a
/// terminal and get the identical launch.
fn quote_arg(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '\n', '\x0b', '"']) {
        return s.into_owned();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Backslashes before a quote are doubled, then the quote is
                // escaped; elsewhere a backslash is literal.
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // A trailing run of backslashes would otherwise escape the closing quote.
    out.extend(std::iter::repeat_n('\\', backslashes));
    out.push('"');
    out
}

/// The full command line the SCM will record for `exe` plus `args`.
pub fn command_line(exe: &Path, args: &[OsString]) -> String {
    let mut out = quote_arg(exe.as_os_str());
    for a in args {
        out.push(' ');
        out.push_str(&quote_arg(a));
    }
    out
}

/// The monitor service's name for a recorder service called `name`.
pub fn monitor_name(name: &str) -> String {
    format!("{name}{MONITOR_SUFFIX}")
}

// ---------------------------------------------------------------------------
// SCM plumbing.
// ---------------------------------------------------------------------------

/// `windows_service::Error`'s `Display` is only ever "IO error in winapi call";
/// the OS error an operator needs is hidden in its `source`.
fn describe(e: &windows_service::Error) -> String {
    match std::error::Error::source(e) {
        Some(src) => format!("{e}: {src}"),
        None => e.to_string(),
    }
}

fn scm(e: windows_service::Error, what: &str) -> Error {
    Error::Other(format!(
        "{what}: {}. (Installing, removing, starting and stopping a service all \
         need an elevated prompt — run this from \"Windows Terminal (Admin)\".)",
        describe(&e)
    ))
}

/// Open the SCM with the access one operation needs.
fn manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| scm(e, "could not open the service control manager"))
}

/// Human name for a start type, for `status`.
///
/// Never says "delayed": `install` sets the delayed-auto-start flag through
/// `ChangeServiceConfig2`, but `QueryServiceConfig` cannot read it back and
/// `windows-service` exposes no getter for it, so `status` reports only what it
/// can actually see. Claiming a flag we did not verify is worse than omitting
/// it — `sc qc <name>` shows the real value.
fn start_type_str(t: ServiceStartType) -> &'static str {
    match t {
        ServiceStartType::AutoStart => "automatic",
        ServiceStartType::OnDemand => "manual",
        ServiceStartType::Disabled => "disabled",
        ServiceStartType::SystemStart => "system",
        ServiceStartType::BootStart => "boot",
    }
}

/// Human name for a service state, for `status`.
fn state_str(s: ServiceState) -> &'static str {
    match s {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "continue-pending",
        ServiceState::PausePending => "pause-pending",
        ServiceState::Paused => "paused",
    }
}

/// What `install` needs to know about one service to register it.
struct Registration {
    name: String,
    display_name: String,
    description: &'static str,
    arguments: Vec<OsString>,
    start_type: ServiceStartType,
    account: Option<OsString>,
}

/// Restart-on-failure policy, applied to every service we install.
///
/// The SCM's own supervision is the outer loop under trunkd's internal
/// supervisor: a device unit that exhausts its restart budget takes the process
/// down, and this brings the process back. Escalating delays keep a genuinely
/// broken install (missing SDR, bad config) from spinning.
fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(30),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(60),
            },
        ]),
    }
}

/// Register one service and configure its description and failure actions.
fn register(exe: &Path, reg: &Registration) -> Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;

    let info = ServiceInfo {
        name: OsString::from(&reg.name),
        display_name: OsString::from(&reg.display_name),
        // Own process: the recorder pins a real-time DSP thread and dlopens SDR
        // drivers, neither of which belongs in a shared svchost.
        service_type: ServiceType::OWN_PROCESS,
        start_type: reg.start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: reg.arguments.clone(),
        // Deliberately no dependencies: the SDRplay API service and WinUSB
        // devices are not SCM dependencies, and the health monitor must be able
        // to run while the recorder it watches is down.
        dependencies: vec![],
        // LocalSystem by default. USB radios reached through WinUSB and the
        // SDRplay API service both work from session 0, so the service needs no
        // interactive desktop and no stored password.
        account_name: reg.account.clone(),
        account_password: None,
    };

    let service = manager
        .create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| scm(e, &format!("could not create service '{}'", reg.name)))?;

    service
        .set_description(reg.description)
        .map_err(|e| scm(e, "could not set the service description"))?;

    service
        .update_failure_actions(failure_actions())
        .map_err(|e| scm(e, "could not set the service failure actions"))?;
    // Without this the restart actions fire only when the process *crashes*; a
    // clean exit(1) on a fatal config error would sit there stopped forever.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(|e| scm(e, "could not enable failure actions for clean exits"))?;

    if reg.start_type == ServiceStartType::AutoStart {
        // Delayed auto-start: USB enumeration and the SDRplay API service are
        // still settling in the first seconds after boot, and a recorder that
        // starts 30 s later loses nothing.
        service
            .set_delayed_auto_start(true)
            .map_err(|e| scm(e, "could not set delayed auto-start"))?;
    }

    println!(
        "installed service '{}' ({})\n  command: {}\n  start:   {}{}\n  account: {}",
        reg.name,
        reg.display_name,
        command_line(exe, &reg.arguments),
        start_type_str(reg.start_type),
        // Verified here because the call above succeeded; `status` cannot read
        // the flag back, so it never repeats this claim.
        if reg.start_type == ServiceStartType::AutoStart {
            " (delayed; `status` cannot read this flag back — see `sc qc`)"
        } else {
            ""
        },
        reg.account.as_ref().map_or("LocalSystem".to_owned(), |a| a
            .to_string_lossy()
            .into_owned()),
    );
    Ok(())
}

/// `trunkd service install`.
#[allow(clippy::too_many_arguments)]
pub fn install(
    config: &Path,
    name: &str,
    display_name: &str,
    start_type: ServiceStartType,
    account: Option<&str>,
    monitor: bool,
    panel_url: &str,
    server_log_dir: Option<&str>,
) -> Result<()> {
    let config = resolve_config_path(config)?;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| strip_verbatim(&p))
        .map_err(|e| Error::Other(format!("could not resolve this executable's path: {e}")))?;

    register(
        &exe,
        &Registration {
            name: name.to_owned(),
            display_name: display_name.to_owned(),
            description: DESCRIPTION,
            arguments: recorder_arguments(&config),
            start_type,
            account: account.map(OsString::from),
        },
    )?;

    if monitor {
        // Derived from the canonical config path, so a relative
        // `[server] log_dir` resolves the same way it will at run time.
        let log_dir = service_log_dir(&config, server_log_dir);
        register(
            &exe,
            &Registration {
                name: monitor_name(name),
                display_name: MONITOR_DISPLAY_NAME.to_owned(),
                description: MONITOR_DESCRIPTION,
                arguments: crate::monitor::service_arguments(name, panel_url, &log_dir, &config),
                start_type,
                account: account.map(OsString::from),
            },
        )?;
    }

    println!(
        "\nLogs: {}\nStart it with `trunkd service start{}`, and check on it with \
         `trunkd service status`.",
        service_log_dir(&config, server_log_dir).display(),
        if name == DEFAULT_NAME {
            String::new()
        } else {
            format!(" --name {name}")
        }
    );
    Ok(())
}

/// Stop `service` if it is running and wait for it to actually stop.
fn stop_and_wait(service: &windows_service::service::Service, name: &str) -> Result<()> {
    let status = service
        .query_status()
        .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    if status.current_state != ServiceState::StopPending {
        service
            .stop()
            .map_err(|e| scm(e, &format!("could not stop service '{name}'")))?;
    }

    let deadline = Instant::now() + UNINSTALL_STOP_TIMEOUT;
    while Instant::now() < deadline {
        let status = service
            .query_status()
            .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
        if status.current_state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
    Err(Error::Other(format!(
        "service '{name}' did not stop within {}s; it is still finishing calls \
         or it is wedged. Wait, or kill the process, then try again.",
        UNINSTALL_STOP_TIMEOUT.as_secs()
    )))
}

/// Delete one service, stopping it first. A service that is not there is not an
/// error: `uninstall` is expected to be re-runnable.
fn remove(name: &str) -> Result<bool> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(name, access) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    stop_and_wait(&service, name)?;
    service
        .delete()
        .map_err(|e| scm(e, &format!("could not delete service '{name}'")))?;
    println!("removed service '{name}'");
    Ok(true)
}

/// `trunkd service uninstall` — removes the recorder and its monitor, if any.
pub fn uninstall(name: &str) -> Result<()> {
    let recorder = remove(name)?;
    let monitor = remove(&monitor_name(name))?;
    if !recorder && !monitor {
        println!("no service named '{name}' is installed");
    }
    // The SCM keeps a deleted service around until every handle to it closes,
    // which for a just-stopped service can outlive this process by a moment.
    Ok(())
}

/// `trunkd service start`.
pub fn start(name: &str) -> Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(|e| scm(e, &format!("could not open service '{name}'")))?;
    service
        .start(&[] as &[&OsStr])
        .map_err(|e| scm(e, &format!("could not start service '{name}'")))?;
    println!("started service '{name}'");

    // The monitor is deliberately left running by `stop` (it is what notices a
    // stop was clean), but it may have been stopped with the box or by hand.
    // Starting the recorder without its supervisor would quietly leave the
    // deployment half-armed, so bring it back too.
    if let Some(monitor) = start_if_stopped(&manager, &monitor_name(name))? {
        println!("started health monitor '{monitor}'");
    }
    Ok(())
}

/// Start `name` if it exists and is stopped; `None` when there is nothing to do.
fn start_if_stopped(manager: &ServiceManager, name: &str) -> Result<Option<String>> {
    let access = ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let Ok(service) = manager.open_service(name, access) else {
        return Ok(None);
    };
    let st = service
        .query_status()
        .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
    if st.current_state != ServiceState::Stopped {
        return Ok(None);
    }
    service
        .start(&[] as &[&OsStr])
        .map_err(|e| scm(e, &format!("could not start service '{name}'")))?;
    Ok(Some(name.to_owned()))
}

/// `trunkd service stop`.
pub fn stop(name: &str) -> Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .map_err(|e| scm(e, &format!("could not open service '{name}'")))?;
    stop_and_wait(&service, name)?;
    println!("stopped service '{name}'");

    // Deliberately does *not* stop the monitor: it reads the SCM's recorded
    // exit code, sees a clean stop, and leaves the recorder alone. Saying so
    // here is the difference between "it stayed down, good" and an operator
    // waiting for a restart that is never coming.
    if monitor_installed(&manager, name) {
        println!(
            "the health monitor '{}' is still running and will leave it stopped \
             (a clean stop is not a fault); start it again with \
             `trunkd service start{}`",
            monitor_name(name),
            if name == DEFAULT_NAME {
                String::new()
            } else {
                format!(" --name {name}")
            }
        );
    }
    Ok(())
}

/// Whether `<name>-monitor` is registered.
fn monitor_installed(manager: &ServiceManager, name: &str) -> bool {
    manager
        .open_service(monitor_name(name), ServiceAccess::QUERY_STATUS)
        .is_ok()
}

/// `trunkd service status` — the recorder and its monitor, when installed.
pub fn status(name: &str) -> Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG;
    let mut found = false;
    for n in [name.to_owned(), monitor_name(name)] {
        let Ok(service) = manager.open_service(&n, access) else {
            continue;
        };
        found = true;
        let st = service
            .query_status()
            .map_err(|e| scm(e, &format!("could not query service '{n}'")))?;
        let cfg = service
            .query_config()
            .map_err(|e| scm(e, &format!("could not read the config of service '{n}'")))?;
        println!("{n}: {}", state_str(st.current_state));
        println!(
            "  pid:     {}",
            st.process_id.map_or("-".to_owned(), |p| p.to_string())
        );
        println!("  start:   {}", start_type_str(cfg.start_type));
        println!(
            "  account: {}",
            cfg.account_name.unwrap_or_default().to_string_lossy()
        );
        println!("  command: {}", cfg.executable_path.display());
    }
    if !found {
        println!("no service named '{name}' is installed");
    }
    Ok(())
}

/// Report the current state of `name`, and the exit code the SCM recorded for
/// its last stop, for the health monitor's rule 4.
///
/// The exit code is what separates "the operator stopped it" from "it died":
/// a clean stop leaves `Win32(0)`, while a crash or a kill leaves a real error
/// (1067 `ERROR_PROCESS_ABORTED`, typically) or a service-specific code.
///
/// `None` means "no such service": the monitor treats that as "nothing to
/// supervise" rather than as a fault.
pub fn query_state(name: &str) -> Result<Option<(ServiceState, ServiceExitCode)>> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) = manager.open_service(name, ServiceAccess::QUERY_STATUS) else {
        return Ok(None);
    };
    let st = service
        .query_status()
        .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
    Ok(Some((st.current_state, st.exit_code)))
}

/// Stop `name` (waiting up to `timeout`) and start it again.
///
/// Used by the health monitor. Kept here so every SCM call in the binary goes
/// through the same error formatting and access flags.
pub fn restart(name: &str, timeout: Duration) -> Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START;
    let service = manager
        .open_service(name, access)
        .map_err(|e| scm(e, &format!("could not open service '{name}'")))?;

    let status = service
        .query_status()
        .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
    if !matches!(
        status.current_state,
        ServiceState::Stopped | ServiceState::StopPending
    ) {
        service
            .stop()
            .map_err(|e| scm(e, &format!("could not stop service '{name}'")))?;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let st = service
            .query_status()
            .map_err(|e| scm(e, &format!("could not query service '{name}'")))?;
        if st.current_state == ServiceState::Stopped {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::Other(format!(
                "service '{name}' did not stop within {}s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(POLL);
    }

    service
        .start(&[] as &[&OsStr])
        .map_err(|e| scm(e, &format!("could not start service '{name}'")))
}

// ---------------------------------------------------------------------------
// Being hosted by the SCM.
// ---------------------------------------------------------------------------

/// The work an SCM-hosted process performs: given the stop flag and a callback
/// to report `Running`, block until finished.
///
/// One shape for both hosted services (the recorder and the health monitor), so
/// there is exactly one piece of SCM lifecycle code in the binary.
pub type ServiceBody =
    Box<dyn FnOnce(Arc<AtomicBool>, &mut dyn FnMut()) -> Result<()> + Send + 'static>;

struct HostedJob {
    name: String,
    body: ServiceBody,
    /// Where to write an error that happens before logging is up.
    error_file: PathBuf,
}

/// `define_windows_service!` generates a bare `extern "system"` entry point
/// with nowhere to put state, so the job is parked here for it to collect.
static HOSTED: Mutex<Option<HostedJob>> = Mutex::new(None);

define_windows_service!(ffi_service_main, service_main);

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: this process was not started by
/// the SCM, so there is nothing to attach to.
const ERROR_NO_SERVICE_CONTROLLER: i32 = 1063;

/// Run `body` under the SCM as service `name`, blocking until it has stopped.
///
/// Hands `body` **back** when there is no service controller to attach to, so a
/// single command line can serve both roles: the SCM launches
/// `trunkd monitor ...` verbatim, and the same line typed into a terminal has to
/// run in the foreground rather than fail.
///
/// `error_file` receives anything that goes wrong before logging exists — a bad
/// config path, mostly. A service has no console, so without this the operator
/// sees only "the service started and then stopped" with no reason. (The
/// Windows Application event log would be the nicer home, but `windows-service`
/// exposes no event-log API and registering an event source by hand is a
/// registry write plus a `ReportEventW` binding — more moving parts than a text
/// file next to the log the operator is already reading.)
pub fn host_or_return(
    name: &str,
    error_file: PathBuf,
    body: ServiceBody,
) -> Result<Option<ServiceBody>> {
    *HOSTED.lock().expect("HOSTED mutex poisoned") = Some(HostedJob {
        name: name.to_owned(),
        body,
        error_file,
    });
    match service_dispatcher::start(name, ffi_service_main) {
        Ok(()) => Ok(None),
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_NO_SERVICE_CONTROLLER) =>
        {
            let job = HOSTED.lock().expect("HOSTED mutex poisoned").take();
            Ok(job.map(|j| j.body))
        }
        Err(e) => {
            let _ = HOSTED.lock().expect("HOSTED mutex poisoned").take();
            Err(Error::Other(format!(
                "could not connect to the service control manager: {}",
                describe(&e)
            )))
        }
    }
}

/// Append `msg` to the pre-logging error file, best effort.
fn report_early_error(path: &Path, msg: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let when = trunkd_core::Timestamp::now()
            .inner()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let _ = writeln!(f, "{when} trunkd service: {msg}");
    }
    // Also to stderr, in case someone ran this by hand to see what happens.
    eprintln!("trunkd: {msg}");
}

/// The SCM's entry point, on a thread the dispatcher owns.
fn service_main(_arguments: Vec<OsString>) {
    let job = HOSTED
        .lock()
        .expect("HOSTED mutex poisoned")
        .take()
        .expect("host() parks the job before starting the dispatcher");
    let error_file = job.error_file.clone();
    if let Err(msg) = run_hosted(job) {
        report_early_error(&error_file, &msg);
    }
}

fn run_hosted(job: HostedJob) -> std::result::Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));

    // The control handler needs the status handle to report StopPending, but
    // the handle only exists once the handler is registered. A `OnceLock` ties
    // the knot; a control arriving in the sliver before it is filled still sets
    // the stop flag, and the `Stopped` report at the end covers the rest.
    let handle_slot: Arc<OnceLock<service_control_handler::ServiceStatusHandle>> =
        Arc::new(OnceLock::new());

    let s = stop.clone();
    let slot = handle_slot.clone();
    let status_handle =
        service_control_handler::register(&job.name, move |control| match control {
            // The SCM's stop is service-mode Ctrl+C: set the same flag the console
            // handler sets and let the recorder finish open calls and drain the
            // uplink in its own time.
            ServiceControl::Stop | ServiceControl::Shutdown => {
                s.store(true, Ordering::Relaxed);
                if let Some(h) = slot.get() {
                    let _ = h.set_service_status(pending_status());
                }
                ServiceControlHandlerResult::NoError
            }
            // Every service has to answer Interrogate, even as a no-op.
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })
        .map_err(|e| format!("could not register the control handler: {}", describe(&e)))?;
    let _ = handle_slot.set(status_handle);

    // Anything after this point that fails must still tell the SCM we stopped,
    // or the service sits in "starting" until it is killed.
    let report = |state: ServiceState, exit: ServiceExitCode| {
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: match state {
                ServiceState::Running => {
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
                }
                _ => ServiceControlAccept::empty(),
            },
            exit_code: exit,
            checkpoint: 0,
            wait_hint: match state {
                ServiceState::StartPending | ServiceState::StopPending => STOP_WAIT_HINT,
                _ => Duration::default(),
            },
            process_id: None,
        });
    };

    report(ServiceState::StartPending, ServiceExitCode::NO_ERROR);

    let mut on_running = || report(ServiceState::Running, ServiceExitCode::NO_ERROR);
    let outcome = (job.body)(stop, &mut on_running);

    match &outcome {
        Ok(()) => report(ServiceState::Stopped, ServiceExitCode::NO_ERROR),
        Err(e) => {
            // Logging may never have come up (a bad config path is the usual
            // case), so this goes to the error file too.
            report_early_error(&job.error_file, &e.to_string());
            report(
                ServiceState::Stopped,
                ServiceExitCode::ServiceSpecific(EXIT_CODE_FAILED),
            );
        }
    }
    Ok(())
}

/// The `StopPending` report sent from the control handler.
fn pending_status() -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: STOP_WAIT_HINT,
        process_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trunkd-service-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_missing_config_is_refused_rather_than_registered() {
        let dir = scratch("config");
        let missing = dir.join("nope.toml");
        let err = resolve_config_path(&missing).expect_err("missing config");
        assert!(err.to_string().contains("config file not found"), "{err}");

        // A relative path that does not resolve from the *current* directory is
        // exactly the case that would break at service start.
        assert!(resolve_config_path(Path::new("definitely-not-here.toml")).is_err());

        let present = dir.join("trunkd.toml");
        std::fs::write(&present, b"# config").expect("write config");
        let resolved = resolve_config_path(&present).expect("resolves");
        assert!(resolved.is_absolute());
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "the verbatim prefix is stripped for readability: {}",
            resolved.display()
        );
        assert!(resolved.ends_with("trunkd.toml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_dir_follows_server_log_dir_and_falls_back_beside_the_config() {
        let cfg = Path::new(r"C:\ProgramData\trunkd\trunkd.toml");

        // Nothing configured: a `logs` folder beside the config.
        assert_eq!(
            service_log_dir(cfg, None),
            PathBuf::from(r"C:\ProgramData\trunkd\logs")
        );
        // Blank is the same as unset, not a directory called "".
        assert_eq!(
            service_log_dir(cfg, Some("   ")),
            PathBuf::from(r"C:\ProgramData\trunkd\logs")
        );

        // An absolute `[server] log_dir` wins outright — this is the case that
        // used to split the recorder and its monitor across two directories.
        assert_eq!(
            service_log_dir(cfg, Some(r"C:\trunkd\logs")),
            PathBuf::from(r"C:\trunkd\logs")
        );

        // A relative one resolves against the *config's* directory, which is
        // the working directory the service sets, so it means the same thing
        // as it does when trunkd is run by hand from there.
        assert_eq!(
            service_log_dir(cfg, Some("logs")),
            PathBuf::from(r"C:\ProgramData\trunkd\logs")
        );
        assert_eq!(
            service_log_dir(cfg, Some(r"var\log")),
            PathBuf::from(r"C:\ProgramData\trunkd\var\log")
        );

        // A bare file name has no parent; the current directory is the only
        // sensible answer and beats panicking.
        assert_eq!(
            service_log_dir(Path::new("trunkd.toml"), None),
            Path::new(".").join("logs")
        );
    }

    #[test]
    fn the_command_line_quotes_paths_with_spaces() {
        let exe = Path::new(r"C:\Program Files\trunkd\trunkd.exe");
        let args = recorder_arguments(Path::new(r"C:\ProgramData\trunkd\trunkd.toml"));
        assert_eq!(
            command_line(exe, &args),
            r#""C:\Program Files\trunkd\trunkd.exe" service run --config C:\ProgramData\trunkd\trunkd.toml"#
        );

        let spaced = recorder_arguments(Path::new(r"C:\My Configs\trunkd.toml"));
        assert_eq!(
            command_line(Path::new(r"C:\trunkd\trunkd.exe"), &spaced),
            r#"C:\trunkd\trunkd.exe service run --config "C:\My Configs\trunkd.toml""#
        );
    }

    #[test]
    fn quoting_survives_trailing_backslashes_and_embedded_quotes() {
        // A quoted argument ending in a backslash must not escape its own
        // closing quote — the classic CommandLineToArgvW trap.
        assert_eq!(quote_arg(OsStr::new(r"C:\a b\")), r#""C:\a b\\""#);
        assert_eq!(quote_arg(OsStr::new(r#"a "b" c"#)), r#""a \"b\" c""#);
        assert_eq!(quote_arg(OsStr::new("plain")), "plain");
        assert_eq!(quote_arg(OsStr::new("")), r#""""#);
    }

    #[test]
    fn the_monitor_service_is_named_after_the_recorder() {
        assert_eq!(monitor_name("trunkd"), "trunkd-monitor");
        assert_eq!(monitor_name("trunkd-west"), "trunkd-west-monitor");
    }
}
