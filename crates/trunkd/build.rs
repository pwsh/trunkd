// SPDX-License-Identifier: GPL-3.0-or-later
//! Build-time provenance for the About page (`GET /about`, `GET /api/about`).
//!
//! Everything a deployed recorder cannot work out for itself — which commit it
//! was built from, when, by which compiler, with which features, and the exact
//! dependency versions the resolver picked — is stamped in here as
//! `cargo:rustc-env` variables plus one generated table in `OUT_DIR`.
//!
//! # It must never break a build
//!
//! trunkd is built from a git checkout on Linux and from an unpacked tarball on
//! the Windows recorder, where there is no `.git` and possibly no `git` at all.
//! Every probe below degrades to `"unknown"` rather than failing: provenance is
//! a diagnostic, and a diagnostic that stops you shipping is worse than no
//! diagnostic.
//!
//! # Staying current
//!
//! A build script that runs only when its own source changes would report the
//! commit of whenever it last ran. The `rerun-if-changed` lines cover the
//! workspace lockfile and — resolved through `git rev-parse`, so a linked
//! worktree's `.git` *file* is handled like a real `.git` directory — `HEAD`,
//! the ref file `HEAD` points at, and the index, which between them are what
//! move on a commit, a checkout, a rebase or a `git add`.
//!
//! `"unknown"` means "git could not be asked", and nothing else: a clean tree
//! reports `git_dirty: false`. That is worth stating because it was not always
//! so — `git status --porcelain` prints nothing for a clean tree, and the
//! generic helper below folds empty output into failure, so the flag read
//! `"unknown"` on every clean build. See [`git_dirty`].

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "src/about/lockparse.rs"]
mod lockparse;

#[path = "src/about/gitprobe.rs"]
mod gitprobe;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/about/lockparse.rs");
    println!("cargo:rerun-if-changed=src/about/gitprobe.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR", "."));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest.clone());

    emit_git(&workspace);
    emit_lock(&workspace);
    emit_build_facts();
    emit_fixture_cfg(&workspace);
}

// ---------------------------------------------------------------------------
// test fixture
// ---------------------------------------------------------------------------

/// `cfg(trunkd_fixture_missing)` when the capture `tests/fixture_pipeline.rs`
/// replays is not on this machine.
///
/// The capture is 2.4 GB and gitignored (`fixtures/README.md`); where it is
/// absent the replay tests are marked `#[ignore]` with the fixture they need,
/// so `cargo t` reports them as ignored instead of as passes that checked
/// nothing. The search order is the tests' own: `$TRUNKD_FIXTURES`, then the
/// workspace's `fixtures/` (a git worktree has none of its own: point
/// `TRUNKD_FIXTURES` at the corpus). The file is `TRUNKD_FIXTURE_NAME` when
/// that is set, else the first named `wb855500k_20M_int8_run2.iq` or
/// `<anything>_wb855500k_20M_int8_run2.iq` (see `find_fixture`).
fn emit_fixture_cfg(workspace: &Path) {
    println!("cargo:rerun-if-env-changed=TRUNKD_FIXTURES");
    println!("cargo:rerun-if-env-changed=TRUNKD_FIXTURE_NAME");
    println!("cargo:rustc-check-cfg=cfg(trunkd_fixture_missing)");
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("TRUNKD_FIXTURES") {
        roots.push(PathBuf::from(dir));
    }
    roots.push(workspace.join("fixtures"));
    // Existing directories only, for the reason `watch` gives.
    for root in roots.iter().filter(|r| r.is_dir()) {
        println!("cargo:rerun-if-changed={}", root.display());
    }
    if !roots.iter().any(|r| find_fixture(r).is_some()) {
        println!("cargo:rustc-cfg=trunkd_fixture_missing");
    }
}

/// The replay capture in `root`, as `tests/fixture_pipeline.rs` looks for it:
/// `TRUNKD_FIXTURE_NAME` exactly, else the first file named `FIXTURE` or
/// `<system prefix>_FIXTURE`.
fn find_fixture(root: &Path) -> Option<PathBuf> {
    const FIXTURE: &str = "wb855500k_20M_int8_run2.iq";
    if let Ok(name) = std::env::var("TRUNKD_FIXTURE_NAME") {
        let p = root.join(name);
        return p.is_file().then_some(p);
    }
    let suffix = format!("_{FIXTURE}");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == FIXTURE || name.ends_with(&suffix)
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// `TRUNKD_GIT_SHA`, `TRUNKD_GIT_DIRTY`, `TRUNKD_GIT_DESCRIBE`, plus the
/// `rerun-if-changed` lines that keep them honest.
fn emit_git(workspace: &Path) {
    let sha = git(workspace, &["rev-parse", "--short=12", "HEAD"]);
    let describe = git(
        workspace,
        &["describe", "--tags", "--always", "--dirty=-dirty"],
    );
    let dirty = git_dirty(workspace);

    println!(
        "cargo:rustc-env=TRUNKD_GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );
    println!(
        "cargo:rustc-env=TRUNKD_GIT_DESCRIBE={}",
        describe.as_deref().unwrap_or("unknown")
    );
    println!(
        "cargo:rustc-env=TRUNKD_GIT_DIRTY={}",
        gitprobe::dirty_str(dirty)
    );

    // Watch HEAD and whatever ref it names, plus the index — which is what
    // moves on a `git add`, a commit or a checkout, and so is what decides
    // whether the dirty flag above is still true.
    //
    // `git rev-parse --git-path` answers with a real path in a plain clone, a
    // submodule and a linked worktree alike, so no `.git`-is-a-directory
    // assumption is baked in here.
    for path in ["HEAD", "ORIG_HEAD", "index"] {
        if let Some(p) = git(workspace, &["rev-parse", "--git-path", path]) {
            watch(workspace, &p);
        }
    }
    if let Some(head_ref) = git(workspace, &["symbolic-ref", "--quiet", "HEAD"]) {
        if let Some(p) = git(workspace, &["rev-parse", "--git-path", &head_ref]) {
            watch(workspace, &p);
        }
        // A packed ref has no file of its own; the pack is what moves.
        if let Some(p) = git(workspace, &["rev-parse", "--git-path", "packed-refs"]) {
            watch(workspace, &p);
        }
    }
}

/// Emit `rerun-if-changed` for a path git named, if it exists.
///
/// A path that does not exist is skipped deliberately: cargo treats a missing
/// `rerun-if-changed` target as "changed", which would rebuild this crate on
/// every single invocation.
fn watch(workspace: &Path, path: &str) {
    let p = Path::new(path);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    if full.exists() {
        println!("cargo:rerun-if-changed={}", full.display());
    }
}

/// Run git in the workspace, returning its trimmed stdout on success.
///
/// `None` covers every way this can fail — no git on PATH, not a repository,
/// an empty repository with no HEAD — and every caller turns that into
/// `"unknown"`.
/// Whether the working tree has uncommitted changes, or `None` when git could
/// not be asked at all (no git on PATH, not a repository — the tarball build on
/// the Windows recorder).
///
/// Deliberately not routed through [`git`], which folds empty output into
/// `None` because for `rev-parse` and `describe` an empty answer *is* a
/// failure. Here empty output is the answer — it is precisely what a clean tree
/// prints — and treating it as failure is what made every clean Linux build
/// report `git_dirty: "unknown"`.
fn git_dirty(workspace: &Path) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(gitprobe::is_dirty(&String::from_utf8_lossy(&out.stdout)))
}

fn git(workspace: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Cargo.lock
// ---------------------------------------------------------------------------

/// Parse the workspace lockfile into `$OUT_DIR/dependencies.rs`.
fn emit_lock(workspace: &Path) {
    let lock = workspace.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());

    let packages = std::fs::read_to_string(&lock)
        .map(|t| lockparse::parse_lock(&t))
        .unwrap_or_default();
    let out = PathBuf::from(env("OUT_DIR", ".")).join("dependencies.rs");
    std::fs::write(&out, lockparse::generate_table(&packages))
        .unwrap_or_else(|e| panic!("could not write {}: {e}", out.display()));
}

// ---------------------------------------------------------------------------
// Compiler, target, profile, features, builder
// ---------------------------------------------------------------------------

/// The facts cargo already knows plus the ones only a subprocess can answer.
fn emit_build_facts() {
    println!("cargo:rustc-env=TRUNKD_BUILD_TIMESTAMP={}", build_time());
    println!("cargo:rustc-env=TRUNKD_RUSTC_VERSION={}", rustc_version());
    println!("cargo:rustc-env=TRUNKD_TARGET={}", env("TARGET", "unknown"));
    println!(
        "cargo:rustc-env=TRUNKD_BUILD_HOST_TRIPLE={}",
        env("HOST", "unknown")
    );
    println!(
        "cargo:rustc-env=TRUNKD_PROFILE={}",
        env("PROFILE", "unknown")
    );
    println!(
        "cargo:rustc-env=TRUNKD_OPT_LEVEL={}",
        env("OPT_LEVEL", "unknown")
    );
    println!("cargo:rustc-env=TRUNKD_FEATURES={}", features());
    println!("cargo:rustc-env=TRUNKD_BUILD_HOST={}", build_host());
}

/// The build timestamp as RFC 3339 UTC, honouring `SOURCE_DATE_EPOCH` so a
/// reproducible build stays reproducible.
fn build_time() -> String {
    let now = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(time::OffsetDateTime::from_unix_timestamp)
        .and_then(Result::ok)
        .unwrap_or_else(time::OffsetDateTime::now_utc);
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// `rustc --version`, asking the compiler cargo is actually using.
fn rustc_version() -> String {
    Command::new(env("RUSTC", "rustc"))
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The enabled cargo features, comma-separated and lowercased.
///
/// Cargo exports one `CARGO_FEATURE_<NAME>` per enabled feature with `-`
/// mapped to `_`; there is no variable listing them, so the environment is
/// swept for the prefix.
fn features() -> String {
    let mut names: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_owned))
        .map(|f| f.to_lowercase().replace('_', "-"))
        .collect();
    names.sort();
    names.join(",")
}

/// The machine that ran this build, for a binary that later turns up somewhere
/// nobody remembers deploying it from.
fn build_host() -> String {
    // No hostname crate and no libc here: a build script should not grow a
    // dependency tree for one string. These are the files and variables that
    // answer on the platforms trunkd is built on.
    for path in ["/etc/hostname", "/proc/sys/kernel/hostname"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim().to_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    // Not `HOST`: cargo sets that to the build *triple*, not the machine.
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(s) = std::env::var(var) {
            let s = s.trim().to_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".to_owned()
}

/// An environment variable with a fallback.
fn env(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_owned())
}
