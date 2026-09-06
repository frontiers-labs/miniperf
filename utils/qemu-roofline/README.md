# QEMU Roofline plugin

This crate provides instruction and memory accounting for miniperf's QEMU
Roofline backend. It builds a QEMU TCG plugin shared library and targets plugin
API 6, as shipped by the repository's QEMU 11.0.2 bundle.

Miniperf runs the guest twice:

1. QEMU without the plugin supplies host-observed duration and PMU samples.
2. QEMU with the plugin counts executed operations and successful architectural
   load/store bytes.

Both runs feed the same Roofline result schema used by the compiler backend.

## Accounting

- x86 scalar and packed floating-point instructions are classified from QEMU
  disassembly. XMM, YMM, and ZMM operations are scaled by register width.
- RISC-V scalar and vector operations are classified by a build-generated
  table derived from the vendored TIR checked-AST specification.
- RVV arithmetic is scaled at execution time using `vl`, `vstart`,
  `vtype.vsew`, and active `v0` mask bits.
- Widening operations use the destination element width and fused
  multiply-add operations count as two operations per active element.
- Missing mandatory RVV state is reported as an error instead of falling back
  to one operation per vector instruction.
- Missing or semantically unclassified TIR operations count as zero and are
  reported through `unclassified_instructions` and plugin diagnostics.
- Memory callbacks count successful architectural guest accesses.

Accounting currently covers the whole guest process, including its dynamic
loader and libraries, and produces one aggregate row. QEMU duration is emulator
time on the host; it is suitable for validating instruction accounting and
arithmetic intensity, but does not predict execution time on RISC-V hardware.

## Building

Build miniperf and the plugin from the workspace root:

```sh
cargo build --release -p mperf -p miniperf-qemu-roofline
```

The plugin is written to
`target/release/libminiperf_qemu_roofline.so`. Miniperf finds it next to the
executable by default, or it can be selected with `--qemu-plugin`.

To build plugin-enabled QEMU user-mode executables for one platform
(`linux-x86_64`, `linux-aarch64`, or `linux-riscv64`):

```sh
utils/build-qemu-user-bundle.sh linux-x86_64 dist
```

The QEMU version comes from `deps/manifest.toml` and the source archive is
verified by SHA-256. The resulting archive contains `qemu-x86_64`,
`qemu-riscv32`, `qemu-riscv64`, `qemu-plugin.h`, QEMU licenses, a dependency
manifest, and a checksum. `.github/workflows/deps.yml` builds it for every
platform and publishes it as a pinned prerelease.

See the [Roofline tutorial](https://frontiers-labs.github.io/miniperf/tutorials/roofline.html) for
complete recording examples.
