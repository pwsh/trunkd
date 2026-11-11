// SPDX-License-Identifier: GPL-3.0-or-later
//! Tracing setup: console logging plus optional rolling file logging.

use std::io::IsTerminal;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize logging.
///
/// `level` is the default filter (e.g. `"info"`); `RUST_LOG` overrides it.
/// When `log_dir` is set, a daily-rolling `file_name` is written there in
/// addition to the console.
///
/// `file_name` is a parameter rather than a constant because more than one
/// trunkd process can be pointed at the same directory: the recorder writes
/// `trunkd.log.<date>` and its health monitor writes
/// `trunkd-monitor.log.<date>`. Two appenders sharing one path would interleave
/// and clobber each other's lines.
///
/// Returns a guard that must be kept alive for the lifetime of the process so
/// the non-blocking file writer flushes.
pub fn init(level: &str, log_dir: Option<&str>, file_name: &str) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Colour only on a terminal. Under systemd stderr is appended to a file
    // (or journald), where the escape codes were almost half of every byte
    // written: 2026-09-18 logged 22.3 MB carrying 12.3 MB of text (review
    // 2026-09-22 §3.5, T-C3).
    let console = fmt::layer()
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);

    let (file_layer, guard) = match log_dir {
        Some(dir) => {
            let appender = tracing_appender::rolling::daily(dir, file_name);
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer().with_ansi(false).with_writer(nb);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .init();

    guard
}
