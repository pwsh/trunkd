// SPDX-License-Identifier: GPL-3.0-or-later
// Locates the optional clear-call JMBE oracle vectors for `tests/jmbe_oracle.rs`.
//
// The clear pair (`imbe_frames_clear.hex` + `jmbe_pcm_clear.i16`) is real
// off-air speech and is not part of every checkout (see `tests/data/README.md`).
// Search order: `tests/data/`, then `$TRUNKD_MBE_VECTORS`. When both files are
// found, `TRUNKD_MBE_CLEAR_DIR` is set to the directory holding them; when
// either is missing, `cfg(trunkd_mbe_clear_missing)` is set and the tests that
// need them are reported **ignored** with the reason, never as silent passes.

use std::path::PathBuf;

const CLEAR_FILES: [&str; 2] = ["imbe_frames_clear.hex", "jmbe_pcm_clear.i16"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TRUNKD_MBE_VECTORS");
    println!("cargo:rustc-check-cfg=cfg(trunkd_mbe_clear_missing)");

    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let mut roots = vec![manifest.join("tests").join("data")];
    if let Some(dir) = std::env::var_os("TRUNKD_MBE_VECTORS") {
        roots.push(PathBuf::from(dir));
    }

    // Watch existing directories so adding or removing the vectors triggers a
    // rebuild (cargo treats a missing rerun-if-changed path as always dirty).
    for root in roots.iter().filter(|r| r.is_dir()) {
        println!("cargo:rerun-if-changed={}", root.display());
    }

    let found = roots
        .iter()
        .find(|root| CLEAR_FILES.iter().all(|name| root.join(name).is_file()));

    match found {
        Some(dir) => println!("cargo:rustc-env=TRUNKD_MBE_CLEAR_DIR={}", dir.display()),
        None => println!("cargo:rustc-cfg=trunkd_mbe_clear_missing"),
    }
}
