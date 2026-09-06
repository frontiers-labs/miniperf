# Install miniperf

## Download a release package

Each GitHub release attaches one archive per platform, with a `.sha256` file next to it.

| Platform | Archive | Contents |
|---|---|---|
| Linux x86-64 | `miniperf-<version>-x86_64-unknown-linux-gnu.tar.gz` | `mperf`, `mperf-gui`, shims, QEMU, DynamoRIO |
| Linux Arm64 | `miniperf-<version>-aarch64-unknown-linux-gnu.tar.gz` | `mperf`, `mperf-gui`, shims, QEMU, DynamoRIO |
| Linux RISC-V 64 | `miniperf-<version>-riscv64gc-unknown-linux-gnu.tar.gz` | `mperf`, shims, QEMU, DynamoRIO. No GUI. Built for `rv64gcv_zba_zbb`. |
| macOS Arm64 | `miniperf-<version>-aarch64-apple-darwin.tar.gz` | `mperf`, `mperf-gui.app`, collector library |
| Windows x86-64 | `miniperf-<version>-x86_64-pc-windows-msvc.zip` | `mperf-gui.exe` only |

Unpack the archive anywhere and add its `bin` directory to `PATH`:

```sh
tar xzf miniperf-*-x86_64-unknown-linux-gnu.tar.gz
export PATH="$PWD/miniperf-<version>-x86_64-unknown-linux-gnu/bin:$PATH"
mperf doctor
```

Linux packages are self-contained. The `lib/miniperf` directory holds the collector library, the libc shim, the QEMU plugin, a plugin-enabled `qemu-user` build, and DynamoRIO with the miniperf client. `mperf` finds them relative to its own executable, so the `roofline` and `mem` scenarios work without installing anything system-wide.

On macOS, drag `mperf-gui.app` into `/Applications`, or run `bin/mperf-gui <directory>` from the unpacked archive. Recording on macOS uses Apple's kperf interface and usually needs `sudo`. See [macOS and Windows](../platforms/desktop.md).

Every package also contains `MANIFEST.txt` with the version, target, source revision, and the dependency release it embeds.

## Build from source

You need:

- A Rust toolchain, 1.85 or newer. Install it from [rustup.rs](https://rustup.rs).
- A C compiler on `PATH` as `cc`. The `mperf` build compiles a small smoke-test program with it.
- On Linux, for the GUI only: `libxcb1-dev`, `libxkbcommon-dev`, and `libxkbcommon-x11-dev`, or your distribution's equivalents.
- On macOS, for the GUI only: Xcode with its command-line tools.

Then:

```sh
git clone https://github.com/frontiers-labs/miniperf.git
cd miniperf
utils/deps/setup-duckdb.sh
cargo build --release -p mperf
```

The binary is `target/release/mperf`. The setup script downloads a pinned DuckDB library into `deps/cache`. It has to run before cargo, and re-running it is a no-op; skipping it fails with `could not find native static library duckdb_static`.

Build the other pieces as you need them:

| Component | Command | Needed for |
|---|---|---|
| Desktop viewer | `cargo build --release -p mperf-gui` | [View results in the GUI](gui.md) |
| Collector library | `cargo build --release -p miniperf-collector-core` | [Trace your code](tracing.md) |
| libc shim | `cargo build --release -p miniperf-shim-libc` | allocation tracking in `mem` |
| QEMU plugin | `cargo build --release -p miniperf-qemu-roofline` | `roofline` and `mem` through QEMU |

If Cargo cannot find the macOS SDK when building the GUI, run the build with `SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"`.

To produce the same archive CI produces, run the packaging script with your Rust target:

```sh
utils/package-miniperf.sh "$(rustc -vV | sed -n 's/^host: //p')" dist
```

### Get QEMU and DynamoRIO for a source build

The `roofline` and `mem` scenarios replay the program under QEMU or DynamoRIO to account every instruction and memory reference. A source build does not include either. Download the pinned bundles that releases embed:

```sh
utils/deps/fetch.sh qemu linux-x86_64 deps/bundles
utils/deps/fetch.sh dynamorio linux-x86_64 deps/bundles
```

Then point `mperf` at them, either with the record options or with environment variables:

```sh
export MPERF_QEMU=deps/bundles/miniperf-qemu-user-*/bin/qemu-x86_64
export MPERF_QEMU_PLUGIN=target/release/libminiperf_qemu_roofline.so
export MPERF_DYNAMORIO=deps/bundles/miniperf-dynamorio-*/dynamorio/bin64/drrun
export MPERF_DR_CLIENT=deps/bundles/miniperf-dynamorio-*/libdr_roofline.so
```

Any QEMU user-mode binary whose `--help` lists `-plugin` also works. Distribution packages often build QEMU without plugin support, which is why miniperf pins its own.

### Optional: the Clang instrumentation pass

The compiler Roofline backend instruments source loops at build time. It needs LLVM 19 or 20 with CMake:

```sh
cmake -S utils/clang_plugin -B target/clang_plugin -GNinja \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_DIR=/path/to/llvm/lib/cmake/llvm
cmake --build target/clang_plugin
```

See [Roofline](scenario-roofline.md) for how to compile a program with it.

## Check the installation

```sh
mperf doctor
```

The next chapters explain what the report means and how to fix each row.
