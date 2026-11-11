# Contributing to trunkd

Thanks for helping. Bug reports, captures of systems that decode badly, and
pull requests are all welcome.

## Reporting a bug

Open an issue with the trunkd version, the operating system, the SDR, and the
relevant part of the log (`RUST_LOG=debug` if you can). For decode problems,
say what the system is (P25 Phase 1, C4FM or simulcast/LSM) and whether
sdrtrunk decodes the same signal. Leave out anything you would not want
public: unit aliases, talkgroup names, keys, tokens, and server addresses.
Security problems go through [`SECURITY.md`](SECURITY.md), not an issue.

## Pull requests

1. Fork, branch from `main`, and keep each pull request to one change.
2. Before you push, run what CI runs:

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo deny check licenses advisories bans sources
   ```

   These are the Linux job of `.github/workflows/ci.yml`; a Windows job also
   builds the release binary and runs `cargo test --workspace --lib`. The
   capture-replay tests need an IQ recording and skip, with a reason, when
   `TRUNKD_FIXTURES` does not point at one, and the clear-voice JMBE tests in
   `trunkd-mbe` skip the same way without their vectors. That is expected.
3. Add or update tests for behaviour you change. Protocol code should cite the
   TIA-102 section or the sdrtrunk class it follows.
4. Every new `.rs` file starts with
   `// SPDX-License-Identifier: GPL-3.0-or-later`.
5. New dependencies must pass `cargo deny` (`deny.toml` lists the allowed
   licenses). Say in the pull request why the dependency is needed.

Accepted pull requests are applied to the maintainer's working tree and
appear here with the next release, so the merge commit on GitHub may carry a
different hash from your branch. Your authorship is kept.

## Licensing of contributions

trunkd is licensed under GPL-3.0-or-later. By submitting a contribution you
agree that it is licensed under the same terms, and you confirm that you
have the right to submit it. Do not paste code from projects whose license
is not GPL-3.0 compatible. Code ported from another GPL project (sdrtrunk,
JMBE) must say so in the module documentation and get an entry in
[`NOTICE.md`](NOTICE.md).
