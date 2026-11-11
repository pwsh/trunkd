// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether the capture-replay test can run here.
//!
//! `tests/site_fixture.rs` replays a 2.4 GB wideband capture that is
//! gitignored (`fixtures/README.md`). Where the capture is absent this sets
//! `cfg(trunkd_fixture_missing)`, and the test is marked `#[ignore]` with the
//! fixture it needs — so `cargo t` reports it as ignored rather than as a
//! pass that checked nothing. The search order is the test's own:
//! `$TRUNKD_FIXTURES`, then the workspace's `fixtures/` (a git worktree has
//! none of its own: point `TRUNKD_FIXTURES` at the corpus). The file is
//! `TRUNKD_FIXTURE_NAME` when that is set, else the first file named
//! [`FIXTURE`] or `<anything>_`[`FIXTURE`] (a capture is usually prefixed with
//! the system it was taken on).
//!
//! Only `Path` probes: nothing here can fail a build.

use std::path::{Path, PathBuf};

/// The capture `tests/site_fixture.rs` replays, less its system prefix.
const FIXTURE: &str = "wb855500k_20M_int8_run2.iq";

/// The capture in `root`, as `tests/site_fixture.rs` looks for it.
fn find_fixture(root: &Path) -> Option<PathBuf> {
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

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TRUNKD_FIXTURES");
    println!("cargo:rerun-if-env-changed=TRUNKD_FIXTURE_NAME");
    println!("cargo:rustc-check-cfg=cfg(trunkd_fixture_missing)");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("TRUNKD_FIXTURES") {
        roots.push(PathBuf::from(dir));
    }
    roots.push(manifest.join("../../fixtures"));

    // Watch the directories that exist, so adding or removing the capture
    // re-decides. A missing path is not watched: cargo treats one as always
    // changed and would rebuild this crate on every invocation.
    for root in roots.iter().filter(|r| r.is_dir()) {
        println!("cargo:rerun-if-changed={}", root.display());
    }
    if !roots.iter().any(|r| find_fixture(r).is_some()) {
        println!("cargo:rustc-cfg=trunkd_fixture_missing");
    }
}
