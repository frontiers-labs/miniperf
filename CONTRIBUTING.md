# Contributing to miniperf

Run the workspace quality gates before submitting a change:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
python3 utils/platform-cfg-guard.py
```

## Where platform code lives

`libprof` is the only crate allowed to select code at compile time, and inside
it only `src/platform/` and the leaf syscall bindings should need to. The guard
above fails the build when a `#[cfg(target_os = ...)]` or
`#[cfg_attr(..., target_arch = ...)]` appears in `mperf/`, `mperf-gui/`,
`store/` or `mperf-data/` outside `utils/platform-cfg-allowlist.txt`. It runs in
seconds on every pull request, because the alternative is finding out from an
aarch64 or macOS job hours later that the two targets were compiling different
programs.

`cfg!(target_os = ...)` is deliberately allowed: both of its branches compile
everywhere, so it cannot break a target you did not build. Prefer branching on
a libprof capability over branching on the platform at all.

When you add to the profiler:

- **A new data source is one `Source` impl in `libprof`** writing through
  `Sink`, plus one registration line in the scenario that wants it. It must
  compile on hosts that cannot run it and report
  `Availability::Unavailable { reason }` there, so the recording degrades with
  an explanation instead of the build failing.
- **A new hardware facility is a new `Mechanism` behind an existing
  `Feature`** in `libprof::features`, never a new scenario, CLI flag or knob.
  Give it an honest `MeasurementQuality`: a mechanism that answers a different
  question than the one asked is `Estimated`, however precise its hardware is.
- **Anything a source produces goes through `Record`.** Sources do not write
  files; adding a new shape of output means a new `Record` variant and one arm
  in the consumer, not a second writer.

## Profiler truth policy

Every new collector or analysis milestone in plans 02–12 must land with its
truth fixture. Each fixture must document its analytic answer, tolerance,
guarded plan milestone, required privileges, and unsupported platforms. A pure
test must exercise its assertion independently of hardware access; when useful,
include mutation-style evidence that a representative collector error fails.

Hardware-backed tests belong in the `truth` crate as ignored, privilege-aware
integration tests. Run them on controlled hardware using the instructions in
`truth/README.md`; GitHub-hosted runners do not provide a reliable hardware PMU.
Do not weaken or silently skip an assertion after recording has started.

## External binary dependencies

DuckDB, DynamoRIO and qemu-user are not built by the main CI gate. The
`Dependencies` workflow builds them for every platform that supports them,
publishes them as a dated prerelease, and opens a pull request repinning
`deps/manifest.toml`; it also runs monthly to pick up new DynamoRIO commits and
QEMU releases. DuckDB is excluded from that automatic refresh because
`miniperf-store` links it against the `duckdb` crate's pregenerated bindings —
bump the crate and the pin together.

To reproduce one dependency locally:

```sh
utils/build-duckdb-bundle.sh linux-x86_64 dist
utils/build-dynamorio-bundle.sh linux-x86_64 dist
utils/build-qemu-user-bundle.sh linux-x86_64 dist
```

`linux-riscv64` cross-compiles; run `utils/deps/setup-riscv64-sysroot.sh` first
on an amd64 Ubuntu host.

`deps/manifest.toml`'s `[support]` table lists the platforms each dependency
must cover. It is the only place that list exists: the workflow generates its
build matrix from it and refuses to publish a release — or rewrite the
manifest — that does not cover it, and CI re-checks the pinned manifest on
every pull request. Adding a platform means adding it there.

### How the pins are consumed

`store/build.rs` downloads the pinned DuckDB, verifies its checksum and unpacks
it into `deps/cache/duckdb`, which `.cargo/config.toml` points `libduckdb-sys`
at through `DUCKDB_LIB_DIR`. The first build on a machine therefore needs
network access; afterwards a stamp file makes it a no-op. Nothing compiles
DuckDB from source any more.

`utils/package-miniperf.sh` embeds the pinned DynamoRIO and qemu-user bundles
under `lib/miniperf`, where `mperf` finds them relative to its own executable
(`mperf/src/roofline/mod.rs`, `package_path`). `utils/verify-miniperf-package.sh`
fails the build if any of that is missing, so a package that would break on a
user's machine never gets uploaded.
