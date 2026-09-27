# dr-roofline — DynamoRIO backend for roofline/memory accounting

DynamoRIO client emitting the same three artifacts as the QEMU plugin
(`qemu-roofline.counts`, CFG v4, `.memory.json`); all analysis
lives in the shared `miniperf-roofline-core` crate (`utils/roofline-core`),
linked in as a staticlib via the C API in `roofline_core.h`.

DynamoRIO instruments natively, so it only works when the target matches the
host architecture — QEMU remains the cross-architecture path. `mperf` prefers
DynamoRIO automatically when `drrun` and this client are available. On Linux,
an auto-selected DynamoRIO run that fails at runtime retries with QEMU
(`--roofline-backend dynamorio|qemu` overrides and never falls back;
`--dynamorio`, `--dynamorio-client`, `MPERF_DYNAMORIO`,
`MPERF_DR_CLIENT` control discovery).
On Windows, the package ships the native DynamoRIO backend; the bundled QEMU
backend is only available on Linux.

## Build

DynamoRIO must be built from master (releases lack the riscv64 port);
`utils/build-dynamorio-bundle.sh <platform>` does everything at the revision
pinned in `deps/manifest.toml`. Manually:

```sh
cargo build --release -p miniperf-roofline-core
cmake -B build -DDynamoRIO_DIR=<dynamorio-build>/cmake .
cmake --build build
<dynamorio-build>/bin64/drrun -disable_traces -max_bb_instrs 32 \
  -c build/libdr_roofline.so output=out.counts memory-profile=on -- ./app
```

For a Windows x86_64 bundle, run
`utils/build-dynamorio-bundle.sh windows-x86_64` from Git Bash in an MSVC
developer environment. The resulting zip contains `dynamorio/bin64/drrun.exe`
and `dr_roofline.dll`. The pinned dependency release must include that zip and
its checksum before `utils/package-miniperf.sh x86_64-pc-windows-msvc` can
embed it.

When running mperf from a source checkout, the built client is discovered in
any CMake build directory directly below `utils/dr-roofline`; it does not need
to be copied beside the mperf executable. `--dynamorio` accepts either the
launcher or the DynamoRIO build directory, so no `PATH` or `LD_LIBRARY_PATH`
changes are normally needed:

```sh
mperf record --dynamorio <dynamorio-build> --scenario roofline \
  -o results -- ./app
```

Packaged bundles are also self-discovering. `--dynamorio-client` is only
needed when keeping a manually built client outside the source checkout or
bundle.

`-disable_traces -max_bb_instrs 32` are required: the per-instruction
instrumentation can exceed DynamoRIO's block emit limits at default sizes
(observed on riscv64). The mperf driver passes them automatically.

## Instrumentation design

The x86 Roofline pass (`memory-profile=off`) uses one atomic execution counter
per translated basic block. Operation counts, instruction counts, and
architectural load/store bytes are multiplied by those counters at shutdown,
removing dynamic-address and trace-record work from hot loops. Direct successor
topology is registered at translation time; block counts are exact while
conditional edge weights are approximate. This pass intentionally does not
produce modeled DRAM traffic. Use the `mem` scenario (`memory-profile=on`) when
exact addresses, reuse distance, or shared-LLC modeling are required.

Exact-address events are inline-written to a per-thread 1MiB `drx_buf` and
processed in 64K-record batches. RVV operations still use clean calls because
they must read vl/vtype/vstart/v0 from the machine context at execution time.
Non-x86 Roofline accounting retains this buffered path until aggregate-counter
instrumentation is validated on those ports.

## Validation status (Aug 2026, DR master 1fd3603b)

- x86_64: FP operation counts and architectural bytes remain exact. On the
  AVX2/FMA matmul benchmark (256×256, 100 repeats), the timed kernel measured
  0.713 s under DynamoRIO versus 0.171 s native (~4.2×); the former buffered
  design was roughly 20× locally and had measured as high as ~55× elsewhere.
  Fixed launcher/translation overhead was 0.13 s in this short run.
- `memory-profile=on` retains exact dynamic addresses and shared-LLC/reuse
  analysis; it remains substantially slower and is intended for the `mem`
  scenario rather than the normal Roofline pass.

CFG v4 adds per-block `arch_bytes_load`/`arch_bytes_store` next to the
existing modeled-DRAM `bytes_load`/`bytes_store`.

Arithmetic intensity is computed from **architectural** traffic — the bytes the
block's load/store operands move — which is the cache-aware roofline (CARM)
convention and matches what the compiler-instrumentation backend already
reports. The original Williams/Waterman/Patterson roofline instead divides by
DRAM traffic; that is a different model, and it cannot be used here because a
loop whose working set fits in the LLC has zero modeled DRAM traffic, making
its intensity infinite and silently removing exactly the compute-bound loops
the chart exists to show. The two must not be mixed per loop: that would put
different loops on different x-axes and make a single loop's intensity jump
when its working set crosses the LLC.

The `roofline` view exposes `arch_bytes` and `dram_bytes` separately. Exact
address backends provide both, so `arch_bytes / dram_bytes` gives each loop's
cache reuse (1x for a streaming loop, unbounded for a cache-resident one). The
low-overhead x86 Roofline pass leaves `dram_bytes` at zero. Unlike modeled DRAM
stores, architectural bytes are attributed to the issuing block for every access, so
their per-block sum must equal the whole-process totals exactly —
`validate_cfg_totals` enforces that.

Because intensity is architectural, a cache-resident loop legitimately sits
above the DRAM roof, so calibration measures a bandwidth roof per hierarchy
level (`memory_levels` in `info.json`) and the chart draws one line per level.
The triad used for those roofs is easy to get wrong: it needs an explicit
`target_feature` vector width (the crate builds for the x86-64 baseline), must
avoid `f64::mul_add` (a libm call without `+fma`), and must zip rather than
index its slices. With any of those wrong the loop is issue-bound and every
level reports the same figure. Levels whose measured bandwidth does not fall as
the working set grows are dropped rather than plotted.

The `image` start address reported to roofline-core is the runtime start of
the executable text segment, matching the QEMU plugin's
`qemu_plugin_start_code()`. mperf derives each loop's module offset as
`image_start - executable ELF segment link address`, so reporting
DynamoRIO's `main_module->start` (the module base, one page below text for a
PIE) skewed every module offset by the text segment's link address and
symbolized every loop to an unrelated symbol such as `__abi_tag`.
- riscv64 (Banana Pi F3, rv64gcv): RVV ops counted exactly (vl/vtype/vstart/v0
  from the mcontext, SEW-bucketed, masked ops supported); saxpy 20M/800M on
  the nose, zero rvv_state_errors.
- aarch64 (Orion O6): NEON classification exact (matmul 268,435,456 double
  ops); DynamoRIO's `$0x0N` element-size operand style is handled in
  roofline-core.

Known gaps: TMDL spec lacks the riscv bitmanip (Zba/Zbb) extension, so those
count as unclassified on hardware that compiles with them (same gap as the
QEMU path). riscv64/aarch64 have not been revalidated since the
buffered-instrumentation redesign (drx_buf is portable, but the boards should
be rerun). memory-profile=on overhead is dominated by the exact per-line
reuse-distance treap in roofline-core — sampling is the known next
optimization there.

Debug switches: `MPERF_DR_DEBUG_UNCLASSIFIED=1` prints unclassified
instructions at translation time; `MPERF_DR_DEBUG_CLASSIFY=1` prints every
classification.
