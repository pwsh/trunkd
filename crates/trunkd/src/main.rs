// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd` — P25 trunked-radio recorder engine (CLI).
//!
//! A thin shell over the `trunkd` library: parse arguments, load config, set up
//! logging, and hand off to [`trunkd::run_once`]. Everything interesting is in
//! the library so the integration tests can drive it in-process.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use trunkd::config::Config;
use trunkd::{import, logging};
use trunkd_core::{Error, Result};

mod monitor;
#[cfg(windows)]
mod service;

/// Rolling log file the recorder writes. The health monitor writes its own
/// ([`monitor::LOG_FILE`]) so the two can share a directory.
const RECORDER_LOG: &str = "trunkd.log";

#[derive(Parser)]
#[command(name = "trunkd", version, about = "P25 trunked-radio recorder engine")]
struct Cli {
    /// Path to trunkd.toml.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Validate config and enumerate SDR devices, then exit.
    #[arg(long, global = true)]
    check: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the recorder (default).
    Run,
    /// Validate config + enumerate SDR devices, then exit.
    Check,
    /// Parse an sdrtrunk playlist and print an equivalent trunkd.toml to stdout.
    ImportSdrtrunk {
        /// Path to the sdrtrunk playlist XML.
        #[arg(value_name = "PLAYLIST.XML")]
        playlist: PathBuf,
    },
    /// Install and control the Windows service.
    ///
    /// Windows only. On Linux the supported path is a systemd unit — see
    /// docs/install-linux.md.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Watch a running recorder's panel and restart it when it stops recording.
    ///
    /// A separate, tiny process of this same executable: no SDR, no DSP, just
    /// HTTP to the local panel and (on Windows) the service control manager.
    Monitor(monitor::MonitorArgs),
}

/// `trunkd service ...`. Parsed on every platform so `--help` works and the
/// "Windows only" answer is a clear error rather than an unknown subcommand.
#[derive(Subcommand)]
enum ServiceCommand {
    /// Register trunkd as a Windows service (needs an elevated prompt).
    ///
    /// The service runs `trunkd service run --config <PATH>` with the config
    /// path recorded absolutely, restarts itself after 5 s / 30 s / 60 s on
    /// failure, and — because a service has no console — writes a rolling
    /// `trunkd.log` to `[server] log_dir` (relative paths resolve against the
    /// config's directory), defaulting to a `logs` directory beside the config
    /// file. `--monitor` logs to `trunkd-monitor.log` in the same directory.
    ///
    /// The command line, including that log directory, is stored in the service
    /// control manager at install time: changing `[server] log_dir` or
    /// `[server] bind` afterwards needs `trunkd service uninstall` followed by
    /// `install` to take effect.
    Install {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
        /// Name shown in services.msc.
        #[arg(long, default_value = "trunkd P25 recorder")]
        display_name: String,
        /// `auto` (delayed automatic start) or `demand` (manual).
        #[arg(long, value_name = "TYPE", default_value = "auto")]
        start_type: StartType,
        /// Account to run as. Default LocalSystem, which reaches WinUSB radios
        /// and the SDRplay API service from session 0. A named user account
        /// needs its password set afterwards (`sc.exe config <name> password=`).
        #[arg(long, value_name = "USER")]
        account: Option<String>,
        /// Also install `<name>-monitor`, an external health monitor that
        /// restarts the recorder when it stops recording.
        #[arg(long)]
        monitor: bool,
    },
    /// Stop and remove the service (and its health monitor).
    Uninstall {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
    },
    /// Start the service, and its health monitor if one is installed and
    /// stopped.
    Start {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
    },
    /// Stop the service, waiting for open calls to finish.
    ///
    /// Leaves the health monitor running: it reads the exit code the SCM
    /// records, sees a clean stop, and leaves the recorder stopped rather than
    /// restarting it under you.
    Stop {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
    },
    /// Print the service's state, pid, start type and command line.
    Status {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
    },
    /// Run under the service control manager. This is what the SCM launches;
    /// use `trunkd run` in a terminal.
    Run {
        /// SCM key for the service.
        #[arg(long, default_value = "trunkd")]
        name: String,
    },
}

/// `--start-type` values.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum StartType {
    /// Start at boot (delayed, so USB and the SDRplay service settle first).
    Auto,
    /// Start only when asked.
    Demand,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match &cli.command {
        Some(Command::ImportSdrtrunk { playlist }) => run_import(playlist),
        Some(Command::Check) => run_check(cli.config.as_deref()),
        Some(Command::Service { command }) => run_service_command(command, cli.config.as_deref()),
        Some(Command::Monitor(args)) => run_monitor(args, cli.config.as_deref()),
        _ if cli.check => run_check(cli.config.as_deref()),
        _ => run_console(cli.config.as_deref()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("trunkd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Load config from `path` if given, else environment-only.
fn load_config(path: Option<&Path>) -> Result<Config> {
    match path {
        Some(p) => {
            // The recorder is handed a parsed `Config`, never a path, so this
            // is the only layer that can tell the About page which file the
            // running process actually read.
            trunkd::about::record_config_path(p);
            Config::load(p)
        }
        None => Config::load_env_only(),
    }
}

fn run_import(playlist: &Path) -> Result<()> {
    let xml = std::fs::read_to_string(playlist)?;
    let cfg = import::playlist_to_config(&xml)?;
    let toml = import::config_to_toml(&cfg)?;
    print!("{toml}");
    Ok(())
}

fn run_check(config: Option<&Path>) -> Result<()> {
    let cfg = load_config(config)?;
    let (en_sites, en_channels) = (cfg.enabled_sites(), cfg.enabled_channels());
    println!("configuration: OK");
    println!(
        "  devices={} sites={} ({} enabled, {} disabled) channels={} ({} enabled, {} disabled)",
        cfg.device.len(),
        cfg.site.len(),
        en_sites,
        cfg.site.len() - en_sites,
        cfg.channel.len(),
        en_channels,
        cfg.channel.len() - en_channels,
    );
    for d in &cfg.device {
        println!(
            "  device '{}' backend={:?} rate={} center={}",
            d.id,
            d.backend,
            d.sample_rate,
            d.center_frequency
                .map_or("-".to_owned(), |c| format!("{c} Hz"))
        );
    }
    for s in &cfg.site {
        println!(
            "  site '{}/{}' [{}] control_freqs={} pool={}",
            s.system,
            s.site,
            if s.enabled { "enabled" } else { "disabled" },
            s.control_frequencies.len(),
            s.traffic_pool_size
        );
    }
    // `spool_only` no longer means "offline": uploads are held, but the keyup
    // control plane still connects, which is what makes the hold releasable
    // from keyup or the local panel instead of from this file. The one case
    // that is still wholly offline is a held uplink with no URL to connect to.
    let has_url = !cfg.uplink.url.trim().is_empty();
    let (mode, target) = if !cfg.uplink.enabled {
        ("disabled", cfg.uplink.spool_path.clone())
    } else if cfg.uplink.spool_only && has_url {
        (
            "spool-only (uploads held; control plane connected)",
            cfg.uplink.url.clone(),
        )
    } else if cfg.uplink.spool_only {
        (
            "spool-only (no url; control plane off)",
            cfg.uplink.spool_path.clone(),
        )
    } else {
        ("keyup", cfg.uplink.url.clone())
    };
    println!("  uplink {mode} → {target}");

    // The recorder link, printed whether it is on or off: "off" is the answer
    // to "why is my peer not seeing my grants?" as often as a wrong port is.
    if cfg.link.enabled {
        println!(
            "  link enabled bind={} peers={} [{}]",
            cfg.link.bind,
            cfg.link.peers.len(),
            if cfg.link.peers.is_empty() {
                "listening only".to_owned()
            } else {
                cfg.link.peers.join(", ")
            }
        );
        for d in &cfg.device {
            println!(
                "    tuner '{}' remote_grants={}",
                d.id,
                d.accepts_remote_grants()
            );
        }
    } else {
        println!("  link disabled");
    }

    print_ppm_memory(&cfg);

    // "no SoapySDR library" and "SoapySDR is fine, you have no radios plugged
    // in" produce the same empty list, and they need very different fixes.
    match trunkd_sdr::soapy::library_status() {
        Ok(status) => println!("\nSoapySDR: {status}"),
        Err(e) => println!("\nSoapySDR: unavailable — {e}"),
    }

    println!("\nSDR devices available on this host:");
    let descriptors = trunkd_sdr::enumerate_all();
    if descriptors.is_empty() {
        println!("  (none enumerated; iqfile sources are opened by explicit path)");
    }
    for d in descriptors {
        println!("  [{}] {} ({})", d.driver, d.label, d.args);
    }
    Ok(())
}

/// Print what each board remembers of its own frequency error.
///
/// Worth its lines in `check`: a tuner that comes up locked on a correction
/// nobody typed is otherwise unexplained, and a *stale* memory — a board
/// measured months ago, or one carrying a number from before the antenna
/// changed — is exactly the thing an operator wants to see before they go
/// looking for a hardware fault. The age is there for that.
fn print_ppm_memory(cfg: &trunkd::config::Config) {
    use trunkd::ppmmemory;

    let path = ppmmemory::memory_path(cfg);
    println!("\nppm memory ({}):", path.display());
    let mem = match ppmmemory::read(&path) {
        Ok(m) => m,
        Err(e) => {
            println!("  unreadable — {e}");
            println!("  (the recorder ignores this and re-measures; delete it to start clean)");
            return;
        }
    };
    if mem.boards.is_empty() {
        println!("  (no board measured yet)");
        return;
    }
    // Which `[[device]]` each remembered board is, when one of them is it: the
    // key is a serial, and an operator thinks in device ids.
    let device_of = |board: &str| {
        cfg.device
            .iter()
            .find(|d| {
                ppmmemory::board_key(d.backend, d.driver.as_deref(), d.serial.as_deref(), &d.id)
                    == board
            })
            .map(|d| format!(" device '{}' auto_ppm={}", d.id, d.auto_ppm))
            .unwrap_or_default()
    };
    let now_ms = trunkd_core::Timestamp::now().unix_millis();
    for (board, e) in &mem.boards {
        let age = e
            .measured_at_ms()
            .map(|ms| ppmmemory::age_label(ms, now_ms))
            .unwrap_or_else(|| "unknown".to_owned());
        println!(
            "  {board}  ppm={:+.3} config={:+.3} samples={} age={age}{}",
            e.ppm,
            e.config_ppm,
            e.samples,
            device_of(board)
        );
    }
}

/// Load config, start logging, and run the recorder until `stop` is set.
///
/// The console path and the Windows service path both go through here, so the
/// two cannot drift: same config resolution, same logging, same stop flag, same
/// pipeline entry point. The only differences either caller gets to make are
/// how the stop flag is raised (Ctrl+C or an SCM control), a log directory to
/// fall back on when `[server] log_dir` is unset, and a callback fired the
/// moment the pipeline is entered.
fn run_recorder(
    path: Option<&Path>,
    stop: Arc<AtomicBool>,
    log_dir_fallback: Option<&Path>,
    on_running: &mut dyn FnMut(),
) -> Result<()> {
    let mut cfg = load_config(path)?;

    // A service has no console to read, so a run with no `[server] log_dir`
    // would leave no trace at all; the caller's fallback keeps the rolling file
    // unconditional there while the console path stays console-only.
    let log_dir = cfg
        .server
        .log_dir
        .clone()
        .or_else(|| log_dir_fallback.map(|d| d.to_string_lossy().into_owned()));
    let _guard = logging::init(&cfg.server.log_level, log_dir.as_deref(), RECORDER_LOG);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_dir = log_dir.as_deref().unwrap_or("(console only)"),
        cwd = %std::env::current_dir().unwrap_or_default().display(),
        "trunkd starting"
    );
    // One pass of the pipeline per iteration. The loop exists for
    // `POST /api/config/reload`: the panel writes `trunkd.toml`, the run ends
    // through the ordinary teardown, and the *same process* builds the whole
    // pipeline again from the re-read file. Same PID matters — a Windows
    // service that exited and was restarted by the SCM would look like a crash
    // to the health monitor, and `on_running` must therefore be reported to the
    // SCM exactly once, on the first pass.
    let mut announced = false;
    loop {
        tracing::info!(
            devices = cfg.device.len(),
            sites = cfg.site.len(),
            sites_enabled = cfg.enabled_sites(),
            channels = cfg.channel.len(),
            channels_enabled = cfg.enabled_channels(),
            "loaded configuration"
        );

        if !announced {
            on_running();
            announced = true;
        }
        let reload = Arc::new(AtomicBool::new(false));
        let outcome = trunkd::run_once_with_reload(&cfg, stop.clone(), reload.clone());
        // A stop raised during teardown wins: the operator asked for the
        // process to go away, and a queued reload must not resurrect it.
        let reloading = reload.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed);

        // The last thing written before `_guard` drops and flushes the rolling
        // file, which is why it lives here and not in the caller: a service has
        // no console, so "did it shut down cleanly or did it die?" has to be
        // answerable from the log alone, and a log that simply stops part-way
        // through teardown is the signature of a crash.
        match &outcome {
            Ok(s) => tracing::info!(
                calls_started = s.calls_started,
                calls_ended = s.calls_ended,
                uploaded = s.uplink.uploaded,
                spooled = s.uplink.spooled,
                failed = s.uplink.failed,
                "{}",
                if reloading {
                    "recorder reloading"
                } else {
                    "trunkd stopped cleanly"
                }
            ),
            Err(e) => tracing::error!(error = %e, "trunkd stopped with an error"),
        }
        if outcome.is_err() || !reloading {
            return outcome.map(|_| ());
        }

        tracing::info!("reloading configuration");
        match load_config(path) {
            Ok(next) => cfg = next,
            // The file was good when the panel accepted the reload, so this is
            // an edit that landed in the gap. Carrying on with the config
            // already in hand keeps the recorder recording, which is strictly
            // better than exiting over a file the operator is mid-way through.
            Err(e) => tracing::error!(
                error = %e,
                "the configuration no longer loads; continuing with the one already running"
            ),
        }
    }
}

/// `trunkd run` (and the bare `trunkd`): the recorder in a terminal.
fn run_console(path: Option<&Path>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        // A second Ctrl+C while the first is still draining means the operator
        // wants out now; the process is always safe to kill (ARCHITECTURE §6).
        if s.swap(true, Ordering::Relaxed) {
            eprintln!("trunkd: second interrupt; exiting immediately");
            std::process::exit(130);
        }
        eprintln!("trunkd: interrupted; finishing open calls and draining the uplink");
    }) {
        eprintln!("trunkd: could not install signal handler: {e}");
    }

    run_recorder(path, stop, None, &mut || {})
}

/// The `--config` a subcommand requires, since the flag itself is optional.
#[cfg(windows)]
fn require_config(path: Option<&Path>) -> Result<&Path> {
    path.ok_or_else(|| Error::config("this subcommand needs --config <PATH>"))
}

// ---------------------------------------------------------------------------
// `trunkd service ...`
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn run_service_command(_command: &ServiceCommand, _config: Option<&Path>) -> Result<()> {
    Err(Error::unsupported(
        "`trunkd service` is Windows only; on Linux run trunkd under systemd \
         (see docs/install-linux.md) — a unit with `ExecStart=/usr/local/bin/trunkd \
         run --config /etc/trunkd/trunkd.toml` and `Restart=on-failure` is the \
         equivalent of everything this subcommand does.",
    ))
}

#[cfg(windows)]
fn run_service_command(command: &ServiceCommand, config: Option<&Path>) -> Result<()> {
    use windows_service::service::ServiceStartType;

    match command {
        ServiceCommand::Install {
            name,
            display_name,
            start_type,
            account,
            monitor,
        } => {
            let config = require_config(config)?;
            // Both services are shaped by the recorder's own `[server]` block:
            // the monitor polls the panel it is configured to serve, and both
            // log where it says to log. Reading it here means an operator who
            // moved the port or the log directory does not have to remember to
            // repeat themselves on the command line.
            let server = load_config(Some(config)).map(|cfg| cfg.server).ok();
            let panel_url = server
                .as_ref()
                .map(|s| panel_url(&s.bind))
                .unwrap_or_else(|| monitor::DEFAULT_URL.to_owned());
            let log_dir = server.as_ref().and_then(|s| s.log_dir.clone());
            service::install(
                config,
                name,
                display_name,
                match start_type {
                    StartType::Auto => ServiceStartType::AutoStart,
                    StartType::Demand => ServiceStartType::OnDemand,
                },
                account.as_deref(),
                *monitor,
                &panel_url,
                log_dir.as_deref(),
            )
        }
        ServiceCommand::Uninstall { name } => service::uninstall(name),
        ServiceCommand::Start { name } => service::start(name),
        ServiceCommand::Stop { name } => service::stop(name),
        ServiceCommand::Status { name } => service::status(name),
        ServiceCommand::Run { name } => run_hosted_recorder(name, config),
    }
}

/// Turn a `[server] bind` into a URL the monitor can poll.
///
/// `0.0.0.0` and `[::]` mean "every interface", which is not an address you can
/// connect *to*; the monitor runs on the same box, so loopback is the answer.
#[cfg(windows)]
fn panel_url(bind: &str) -> String {
    let port = bind
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(8080);
    format!("http://127.0.0.1:{port}")
}

/// `trunkd service run`: the recorder, hosted by the SCM.
#[cfg(windows)]
fn run_hosted_recorder(name: &str, config: Option<&Path>) -> Result<()> {
    let config = service::resolve_config_path(require_config(config)?)?;
    // The same derivation `install` used, so the directory the SCM command line
    // promised is the one that actually gets written. Loaded here only for
    // `[server] log_dir`; a config that will not parse falls back to
    // `<config dir>\logs`, which is where the error file has to land anyway.
    let configured = load_config(Some(&config))
        .ok()
        .and_then(|c| c.server.log_dir);
    let log_dir = service::service_log_dir(&config, configured.as_deref());
    let error_file = log_dir.join("trunkd-service-error.txt");

    // Services start in C:\Windows\System32. Relative `spool_path` and
    // `event_log_dir` have to mean what they meant when the operator ran trunkd
    // by hand in the config's directory, so move there before loading anything.
    if let Some(dir) = config.parent() {
        std::env::set_current_dir(dir).map_err(|e| {
            Error::Other(format!(
                "could not change to the config's directory {}: {e}",
                dir.display()
            ))
        })?;
    }
    std::fs::create_dir_all(&log_dir)?;

    let body: service::ServiceBody = Box::new(move |stop, on_running| {
        // Only reached when the SCM actually hosts us: `host_or_return` hands
        // the body back unrun when there is no service controller. By the time
        // the recorder is up, a service and a console run are indistinguishable
        // from the inside, so this is the one place that can say which it is.
        trunkd::about::host::note_windows_service();
        run_recorder(Some(&config), stop, Some(&log_dir), on_running)
    });

    match service::host_or_return(name, error_file, body)? {
        None => Ok(()),
        Some(_) => Err(Error::unsupported(
            "`trunkd service run` is what the service control manager launches. \
             To run the recorder in a terminal use `trunkd run --config <PATH>`.",
        )),
    }
}

// ---------------------------------------------------------------------------
// `trunkd monitor`
// ---------------------------------------------------------------------------

/// The health monitor, in a terminal or hosted by the SCM.
///
/// The installed service's command line is the plain `trunkd monitor ...` one,
/// so which of the two this is can only be discovered by trying to attach to
/// the SCM (`host_or_return` hands the work back when there is no controller).
#[cfg(windows)]
fn run_monitor(args: &monitor::MonitorArgs, config: Option<&Path>) -> Result<()> {
    let name = args
        .service
        .as_deref()
        .map(service::monitor_name)
        .unwrap_or_else(|| format!("{}{}", service::DEFAULT_NAME, service::MONITOR_SUFFIX));
    let error_file = args.log_dir.join("trunkd-monitor-error.txt");

    let owned = args.clone();
    let config = config.map(Path::to_path_buf);
    let body: service::ServiceBody =
        Box::new(move |stop, on_running| monitor::run(&owned, config.as_deref(), stop, on_running));

    match service::host_or_return(&name, error_file, body)? {
        None => Ok(()),
        // No SCM: run it in the foreground, Ctrl+C to stop.
        Some(body) => body(console_stop_flag(), &mut || {}),
    }
}

#[cfg(not(windows))]
fn run_monitor(args: &monitor::MonitorArgs, config: Option<&Path>) -> Result<()> {
    monitor::run(args, config, console_stop_flag(), &mut || {})
}

/// A stop flag raised by Ctrl+C, for the foreground modes that are not the
/// recorder (which has its own two-stage handler).
fn console_stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        if s.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
        eprintln!("trunkd: interrupted; stopping");
    }) {
        eprintln!("trunkd: could not install signal handler: {e}");
    }
    stop
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn the_installed_monitor_polls_the_configured_panel_port() {
        assert_eq!(panel_url("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(panel_url("127.0.0.1:9000"), "http://127.0.0.1:9000");
        // A wildcard bind is not an address you can connect to; the monitor
        // runs on the same box, so loopback is the only right answer.
        assert_eq!(panel_url("0.0.0.0:8123"), "http://127.0.0.1:8123");
        assert_eq!(panel_url("[::]:8123"), "http://127.0.0.1:8123");
        // Anything that is not a port falls back to the documented default
        // rather than producing a URL nothing can connect to.
        assert_eq!(panel_url("nonsense"), "http://127.0.0.1:8080");
        assert_eq!(panel_url(""), "http://127.0.0.1:8080");
    }

    #[test]
    fn the_cli_still_parses_every_documented_shape() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
