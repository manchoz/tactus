---
name: crate-release
description: >-
  Checklist for releasing a workspace crate to crates.io from this repo (pre-flight checks across
  feature sets, Cargo.toml/docs.rs metadata, verbatim license files, packaging list, tag, dry-run,
  publish confirmation, post-publish verification, GitHub release). Use when preparing, publishing,
  or patching a crate such as crates/arduino-router.
---

# Crate Release Runbook (crates.io)

Worked example: `crates/arduino-router` v0.1.0 (tag `arduino-router-v0.1.0`).

## 1. Pre-flight (all must pass with zero warnings)
Run for the crate `<crate>` on every feature profile it offers (default, `--no-default-features`, `--all-features`):
- `cargo fmt --check`
- `cargo clippy -p <crate> --all-features --all-targets -- -D warnings` and `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo check -p <crate> --no-default-features` (plus each optional feature on its own)
- `cargo test -p <crate> --no-default-features`, `cargo test -p <crate> --all-features`, `cargo test --workspace`
- `RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps` for each profile (feature-gated intra-doc links break the `--no-default-features` build; use plain backticks for gated items)
- `#![warn(missing_docs)]` in `lib.rs`; check with `RUSTFLAGS="-W missing-docs" cargo check -p <crate> --all-features` (enum variant fields are easy to miss)
- Cross-build the workspace for the target (`cargo zigbuild --workspace --release --target aarch64-unknown-linux-gnu.2.36`) when the crate is consumed by `tactusd`.

## 2. Metadata and files
- `Cargo.toml`: `license` (SPDX), `description`, `repository`, `documentation`, `readme`, `keywords`, `categories`, and `[package.metadata.docs.rs] all-features = true`.
- License files must be **verbatim** standard texts (MIT, Apache-2.0). Never edit them (a modified Apache-2.0 invalidates the SPDX claim; put disclaimers in the README). Once published the tarball is immutable.
- README links must work on crates.io: use absolute GitHub URLs for license files and repo paths, not relative ones. Keep README free of hidden doctest lines (`#`).
- Do not claim an MSRV (`rust-version`) as tested unless it was run on that toolchain.
- `cargo package -p <crate> --list --allow-dirty` must list only intended files (no scratch, logs, `target/`).

## 3. Commit, tag, push
- Stage explicitly (`git add <paths>`); never `git add -A` (untracked local files such as `TOPICS.md` stay out).
- Tag `<crate>-v<version>`; push `main` and the tag (`git push origin main <tag>`).

## 4. Dry run, then publish
- On a clean tree: `cargo publish -p <crate> --dry-run` (needs network; confirms name availability, builds from the packaged tarball).
- Live `cargo publish -p <crate>` is irreversible: stop after the dry run and get explicit user confirmation (or let the user run it with their credentials).
- Published versions cannot be overwritten or deleted. Fix forward with a patch version (`yank` only hides it from new resolves).

## 5. Post-publish
- crates.io: `curl -fsS -A "<tool>/1.0" https://crates.io/api/v1/crates/<crate>` (the API returns 403 without a `User-Agent`).
- docs.rs: `curl -fsSI https://docs.rs/<crate>/<version>/<crate_underscored>/` should return 200 (build can take minutes).
- GitHub release: `gh release view <tag>`; if missing, `gh release create <tag> --title "<crate> v<version>" --notes "…"`.
- CI should exercise the feature matrix and docs (see `.github/workflows/ci.yml`); keep badges in the crate README.

## 6. Environment notes
- `curl`, `gh`, `git push`, `cargo publish` need network and `~/.config/gh`; the sandbox blocks them, so they run unsandboxed (still try sandboxed first per the standing sandbox rule).
- Loopback TCP tests can fail with `PermissionDenied` inside the macOS sandbox; make such tests skip on that error rather than fail.
