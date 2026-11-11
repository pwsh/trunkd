// SPDX-License-Identifier: GPL-3.0-or-later
//! `git status --porcelain` → the About page's `git_dirty` flag.
//!
//! Compiled **twice**, exactly like [`lockparse`](super::lockparse): once by
//! `build.rs` (via `#[path]`), which asks git the question at compile time, and
//! once by the crate itself, where the unit tests live. A build script cannot
//! host `#[test]`s of its own, and this particular answer was wrong for a long
//! time in a way no one noticed.
//!
//! # The bug this exists to keep fixed
//!
//! `--porcelain` prints one line per modified path and **nothing at all** for a
//! clean tree. The build script's general-purpose git helper treats empty
//! output as failure (for `rev-parse` and friends, an empty answer *is* a
//! failure), so every clean Linux build with git right there reported
//! `git_dirty: "unknown"` — the one value that was supposed to mean "there is
//! no git here to ask". Emptiness is the whole signal here, so this question
//! gets its own reader.
//!
//! Nothing here reaches outside `std`; the crate side is otherwise unused,
//! hence the module-wide `dead_code` allowance.
#![allow(dead_code)]

/// Whether a `git status --porcelain` output means the tree has uncommitted
/// changes.
///
/// Empty (or whitespace only) is a clean tree. Anything else is a modified
/// path, so it is dirty.
pub fn is_dirty(porcelain: &str) -> bool {
    !porcelain.trim().is_empty()
}

/// The `TRUNKD_GIT_DIRTY` value for a probe result: a real answer when git
/// could be asked, `"unknown"` only when it could not.
pub fn dirty_str(dirty: Option<bool>) -> &'static str {
    match dirty {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_tree_prints_nothing_and_is_not_dirty() {
        assert!(!is_dirty(""));
        // git writes no trailing newline for a clean tree, but the output is
        // trimmed on the way in from some callers and not others.
        assert!(!is_dirty("\n"));
        assert!(!is_dirty("  \n\t"));
    }

    #[test]
    fn any_modified_path_is_dirty() {
        assert!(is_dirty(" M crates/trunkd/build.rs\n"));
        assert!(is_dirty("A  docs/install-linux.md\nD  old.rs\n"));
        // A rename, and a path with a space in it, are still just lines.
        assert!(is_dirty("R  a.rs -> b.rs\n"));
        assert!(is_dirty("?? some file.txt\n"));
    }

    #[test]
    fn the_flag_says_unknown_only_when_git_could_not_be_asked() {
        // The regression: a clean tree is "false", never "unknown".
        assert_eq!(dirty_str(Some(false)), "false");
        assert_eq!(dirty_str(Some(true)), "true");
        assert_eq!(dirty_str(None), "unknown");
    }
}
