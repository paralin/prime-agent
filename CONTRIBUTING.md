# Contributing to Prime Agent

Read [AGENTS.md](AGENTS.md) before changing the Rust workspace. Each crate owns one area; its README defines scope and public API boundaries. Keep one logical change per pull request and submit it to `PrimeIntellect-ai/prime-agent` with base `main`.

Build with `cargo build --release -p pa-cli`. Run `make check` before merging: formatting, strict Clippy, workspace tests, and the release build. Tests use local servers or scripted providers. The check target disables background catalogs, update checks, and telemetry with `PI_OFFLINE=1 DO_NOT_TRACK=1`.

Record user-visible changes in the workspace [CHANGELOG.md](CHANGELOG.md) or a root `.changes/` entry consumed by the native release preparation tool. Do not create per-crate changelogs or restore Node release tooling. Include actual TypeScript comparison evidence for user-visible changes and state ownership and public API compliance in the pull request description.
