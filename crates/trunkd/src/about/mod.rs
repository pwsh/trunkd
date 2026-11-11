// SPDX-License-Identifier: GPL-3.0-or-later
//! `GET /about` and `GET /api/about`: everything about *this* running binary.
//!
//! # Why a recorder needs an About page
//!
//! A trunkd instance is a headless box somewhere with an antenna on it, often
//! built months ago by someone who has since forgotten which branch they were
//! on. When a call goes missing, the first three questions are always the same:
//! *which build is this*, *which SoapySDR is it using*, and *what is it
//! actually configured with* — and answering them today means SSH, a `git log`
//! on a different machine, and reading a TOML file that may not be the one the
//! process loaded. This page answers all three from the process itself, over
//! the panel that is already there.
//!
//! # The five sections
//!
//! | Section | Source | When it is computed |
//! |---|---|---|
//! | `build` | `build.rs` (`cargo:rustc-env`) | compile time |
//! | `dependencies` | the workspace `Cargo.lock`, parsed by `build.rs` | compile time |
//! | `libraries` | `dlopen`ed native libraries, asked their versions | first request, then cached |
//! | `process` | the OS, per request | every request |
//! | `config` | the loaded [`Config`], redacted | process start |
//!
//! Splitting them by cost is the whole design: the build and dependency tables
//! are `&'static` data, the config is serialized once at startup (so no code
//! path can ever serialize the *unredacted* one), the library probe is cached
//! behind a `OnceLock` because `dlopen` should happen once, and only the cheap
//! per-process facts — uptime, free space — are recomputed per request.
//!
//! # Secrets
//!
//! The config section goes through [`redact`], which blanks any field whose
//! *name* looks like a credential and never inspects a value. See that module
//! for why that direction matters.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use serde_json::{json, Value};

use crate::config::Config;

mod gitprobe;
pub mod host;
mod lockparse;
pub mod redact;
mod runtime;

/// The dependency table `build.rs` baked in from `Cargo.lock`:
/// `(name, version, source, is_workspace_member)`.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/dependencies.rs"));
}

/// The self-contained About page. Inline CSS and JS, like the panel it sits
/// beside: a recorder may have no route to the internet, and a diagnostics page
/// that needs a CDN fails exactly when it is needed.
pub const ABOUT_HTML: &str = include_str!("about.html");

/// The JSON schema version of `/api/about`.
///
/// Bumped when a field changes meaning or disappears; adding a field does not
/// bump it. Anything scraping this endpoint should check it and refuse to
/// guess.
pub const SCHEMA_VERSION: u32 = 1;

/// Where the config was loaded from, recorded by the CLI at load time.
///
/// The recorder is handed a parsed [`Config`], not a path — deliberately, so
/// the integration tests can drive it with a config that was never a file. The
/// path is therefore stashed here by the one layer that knows it, and is simply
/// absent for an env-only or in-process run.
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Record the config file this process loaded. First call wins.
pub fn record_config_path(path: &std::path::Path) {
    let _ = CONFIG_PATH.set(path.to_path_buf());
}

/// The config file path, if this process loaded one.
pub fn config_path() -> Option<&'static std::path::Path> {
    CONFIG_PATH.get().map(PathBuf::as_path)
}

/// Everything the About endpoint serves, assembled once at startup.
///
/// Held by the panel as an `Arc` and never mutated: the config cannot change
/// for the life of the process, so neither can most of this.
pub struct About {
    /// The redacted config, as JSON.
    config_json: Value,
    /// The same config as TOML text, for the page's `<pre>` and for pasting
    /// into a bug report.
    config_toml: String,
    /// Dotted paths the operator's config file actually set, so the page can
    /// separate what was chosen from what was inherited.
    file_keys: Vec<String>,
    /// Directories worth reporting free space for, as `(label, path)`.
    directories: Vec<(String, PathBuf)>,
    /// Wall-clock process start, for `started_at`.
    started_wall: SystemTime,
    /// Monotonic process start, for `uptime_s` — a clock step must not make the
    /// recorder look like it restarted.
    started: Instant,
    /// The native-library probe, run at most once. `dlopen` is not free and its
    /// answer cannot change while the process lives.
    libraries: OnceLock<Value>,
}

impl About {
    /// Build the About state from the config the recorder is running.
    pub fn new(cfg: &Config) -> Arc<About> {
        let path = config_path();
        Arc::new(About {
            config_json: redact::config_json(cfg),
            config_toml: redact::config_toml(cfg),
            file_keys: path
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| redact::keys_present(&t))
                .unwrap_or_default(),
            directories: directories(cfg),
            started_wall: SystemTime::now(),
            started: Instant::now(),
            libraries: OnceLock::new(),
        })
    }

    /// The whole `/api/about` body.
    pub fn json(&self) -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "build": build(),
            "dependencies": dependencies(),
            "libraries": self.libraries.get_or_init(runtime::libraries).clone(),
            "process": self.process(),
            "config": {
                "path": config_path().map(|p| p.display().to_string()),
                "redacted": true,
                "keys_from_file": self.file_keys,
                "env_overrides": env_overrides(),
                "json": self.config_json,
                "toml": self.config_toml,
            },
        })
    }

    /// The per-request `process` section.
    fn process(&self) -> Value {
        let (os_name, os_version, kernel) = host::os_info();
        json!({
            "hostname": host::hostname(),
            "os": os_name,
            "os_version": os_version,
            "kernel": kernel,
            "arch": std::env::consts::ARCH,
            "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "pid": std::process::id(),
            "started_at": rfc3339(self.started_wall),
            "uptime_s": self.started.elapsed().as_secs(),
            "executable": std::env::current_exe().ok().map(|p| p.display().to_string()),
            "working_dir": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "launched_by": host::launch_context(),
            "directories": self.directories.iter().map(|(label, path)| {
                let space = host::disk_space(path);
                json!({
                    "label": label,
                    "path": path.display().to_string(),
                    "exists": path.exists(),
                    "free_bytes": space.map(|(free, _)| free),
                    "total_bytes": space.map(|(_, total)| total),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// The directories a recorder writes to, in the order they matter when a disk
/// fills: calls first, then the event log, then logs.
fn directories(cfg: &Config) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut push = |label: &str, path: &str| {
        let path = path.trim();
        if !path.is_empty() {
            out.push((label.to_owned(), PathBuf::from(path)));
        }
    };
    push("spool", &cfg.uplink.spool_path);
    push("event_log", &cfg.recorder.event_log_dir);
    push("ppm_memory", &cfg.recorder.ppm_memory_path);
    if let Some(dir) = cfg.server.log_dir.as_deref() {
        push("log", dir);
    }
    out
}

/// The `TRUNKD_` environment variables in force, by **name only**.
///
/// Values are deliberately not reported: `TRUNKD_UPLINK__API_KEY` is a
/// perfectly ordinary way to configure a recorder, and a page that printed
/// what it was set to would have handed out the key that the config redaction
/// exists to protect.
fn env_overrides() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("TRUNKD_"))
        .collect();
    names.sort();
    names
}

/// The compile-time `build` section, all of it from `build.rs`.
fn build() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": env!("TRUNKD_GIT_SHA"),
        "git_describe": env!("TRUNKD_GIT_DESCRIBE"),
        // A tri-state, not a bool: a build from a tarball has no way to know
        // whether the tree it came from was clean, and saying "false" there
        // would be a claim rather than an answer.
        "git_dirty": match env!("TRUNKD_GIT_DIRTY") {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => Value::String(other.to_owned()),
        },
        "built_at": env!("TRUNKD_BUILD_TIMESTAMP"),
        "built_on": env!("TRUNKD_BUILD_HOST"),
        "rustc": env!("TRUNKD_RUSTC_VERSION"),
        "target": env!("TRUNKD_TARGET"),
        "host_triple": env!("TRUNKD_BUILD_HOST_TRIPLE"),
        "profile": env!("TRUNKD_PROFILE"),
        "opt_level": env!("TRUNKD_OPT_LEVEL"),
        "features": features(),
        "debug_assertions": cfg!(debug_assertions),
    })
}

/// The enabled cargo features, as a list.
fn features() -> Vec<&'static str> {
    env!("TRUNKD_FEATURES")
        .split(',')
        .filter(|f| !f.is_empty())
        .collect()
}

/// The `dependencies` section: every package in the workspace lockfile.
fn dependencies() -> Value {
    json!({
        "count": generated::DEPENDENCIES.len(),
        "workspace_count": generated::DEPENDENCIES.iter().filter(|d| d.3).count(),
        "packages": generated::DEPENDENCIES.iter().map(|(name, version, source, workspace)| json!({
            "name": name,
            "version": version,
            "source": source,
            "workspace": workspace,
        })).collect::<Vec<_>>(),
    })
}

/// A `SystemTime` as RFC 3339 UTC, or `null` for a clock before the epoch.
fn rfc3339(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let ts = time::OffsetDateTime::from_unix_timestamp(secs as i64).ok()?;
    ts.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_section_answers_every_field_even_without_git() {
        let b = build();
        for key in [
            "version", "git_sha", "built_at", "built_on", "rustc", "target", "profile", "features",
        ] {
            assert!(b.get(key).is_some(), "build section is missing {key}: {b}");
        }
        assert_eq!(b["version"], env!("CARGO_PKG_VERSION"));
        // Either a real sha or the honest fallback, never an empty string.
        let sha = b["git_sha"].as_str().expect("a string");
        assert!(!sha.is_empty());
        assert!(
            b["rustc"]
                .as_str()
                .is_some_and(|r| r.starts_with("rustc ") || r == "unknown"),
            "rustc version looks wrong: {b}"
        );
        // A build from a checkout with git present answers the dirty question
        // properly. `"unknown"` is reserved for a build that could not ask —
        // it is not what a clean tree looks like, which is what it used to be.
        assert!(
            b["git_dirty"].is_boolean() || b["git_dirty"] == "unknown",
            "git_dirty is a bool or the honest fallback: {b}"
        );
        if std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.git")
            .exists()
        {
            assert!(
                b["git_dirty"].is_boolean(),
                "this build had a repository to ask: {b}"
            );
        }
    }

    #[test]
    fn the_dependency_table_is_baked_in_and_names_our_own_crates() {
        let deps = dependencies();
        let count = deps["count"].as_u64().expect("a count");
        assert!(
            count > 10,
            "the lockfile has more than a handful of packages: {count}"
        );
        let packages = deps["packages"].as_array().expect("an array");
        assert_eq!(packages.len() as u64, count);

        let ours = packages
            .iter()
            .find(|p| p["name"] == "trunkd-core")
            .expect("trunkd-core is in the lock");
        assert_eq!(ours["workspace"], true, "a path crate is ours: {ours}");
        assert_eq!(ours["source"], "path");

        let theirs = packages
            .iter()
            .find(|p| p["name"] == "serde")
            .expect("serde is in the lock");
        assert_eq!(theirs["workspace"], false);
        assert_eq!(theirs["source"], "crates.io");
        assert!(
            deps["workspace_count"].as_u64().expect("a count") >= 8,
            "every workspace member is counted: {deps}"
        );
    }

    #[test]
    fn the_feature_list_reflects_how_this_build_was_configured() {
        // The test binary is built with whatever features cargo was given; the
        // point is that the list is populated and parseable, and that a
        // default build reports the SDR backend it has.
        let features = features();
        assert!(
            features.iter().all(|f| !f.is_empty()),
            "no empty entries: {features:?}"
        );
        if cfg!(feature = "soapy") {
            assert!(features.contains(&"soapy"), "{features:?}");
        }
    }

    #[test]
    fn the_body_carries_every_documented_section_and_no_secret() {
        let mut cfg = Config::default();
        cfg.uplink.api_key = "kup_live_notinthebody".into();
        cfg.uplink.spool_path = "spool".into();
        cfg.server.tokens = vec!["panel-token-notinthebody".into()];

        let about = About::new(&cfg);
        let body = about.json();
        for key in [
            "schema_version",
            "build",
            "dependencies",
            "libraries",
            "process",
            "config",
        ] {
            assert!(body.get(key).is_some(), "missing {key}");
        }
        assert_eq!(body["schema_version"], SCHEMA_VERSION);
        assert_eq!(body["config"]["redacted"], true);
        assert!(body["config"]["toml"]
            .as_str()
            .is_some_and(|t| !t.is_empty()));

        let text = body.to_string();
        for secret in ["kup_live_notinthebody", "panel-token-notinthebody"] {
            assert!(!text.contains(secret), "{secret} leaked into /api/about");
        }
    }

    #[test]
    fn the_process_section_describes_this_process() {
        let about = About::new(&Config::default());
        let p = about.json()["process"].clone();
        assert_eq!(p["pid"], std::process::id());
        assert_eq!(p["arch"], std::env::consts::ARCH);
        assert!(p["os"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(p["executable"].as_str().is_some());
        assert!(
            p["started_at"].as_str().is_some(),
            "the clock is after 1970"
        );
        assert!(matches!(
            p["launched_by"].as_str(),
            Some("foreground" | "systemd" | "windows-service")
        ));
    }

    #[test]
    fn the_reported_directories_come_from_the_config_and_carry_free_space() {
        let mut cfg = Config::default();
        cfg.uplink.spool_path = std::env::temp_dir().display().to_string();
        cfg.recorder.event_log_dir = String::new();
        cfg.server.log_dir = Some(std::env::temp_dir().display().to_string());

        let about = About::new(&cfg);
        let dirs = about.json()["process"]["directories"].clone();
        let dirs = dirs.as_array().expect("an array");
        let labels: Vec<_> = dirs.iter().filter_map(|d| d["label"].as_str()).collect();
        assert!(labels.contains(&"spool"), "{labels:?}");
        assert!(labels.contains(&"log"), "{labels:?}");
        assert!(
            !labels.contains(&"event_log"),
            "an empty path is not a directory to report on: {labels:?}"
        );
        let spool = dirs
            .iter()
            .find(|d| d["label"] == "spool")
            .expect("the spool");
        assert_eq!(spool["exists"], true);
        assert!(
            spool["free_bytes"].as_u64().is_some(),
            "an existing directory reports its volume: {spool}"
        );
    }

    #[test]
    fn env_override_names_are_reported_without_their_values() {
        // Set through the process environment, which is what the recorder
        // itself reads; the name has to appear and the value must not.
        // SAFETY: single-threaded test setup, before the read below.
        std::env::set_var("TRUNKD_ABOUT_TEST__SECRET", "value-must-not-appear");
        let names = env_overrides();
        std::env::remove_var("TRUNKD_ABOUT_TEST__SECRET");

        assert!(
            names.contains(&"TRUNKD_ABOUT_TEST__SECRET".to_owned()),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("value-must-not-appear")),
            "only names are reported"
        );
    }

    #[test]
    fn the_page_is_self_contained() {
        // The recorder may have no route out; a diagnostics page that fetches
        // from a CDN is a diagnostics page that fails when it is needed.
        assert!(!ABOUT_HTML.contains("http://"), "no external references");
        assert!(
            !ABOUT_HTML.contains("https://cdn") && !ABOUT_HTML.contains("<script src"),
            "no external scripts"
        );
        assert!(ABOUT_HTML.contains("/api/about"), "the page drives the API");
    }
}
