# Summary

[Introduction](introduction.md)

# Getting started

- [Install miniperf](guide/install.md)
- [Set up permissions](guide/permissions.md)
- [Check your system with mperf doctor](guide/doctor.md)
- [Your first profile](guide/first-profile.md)

# User guide

- [Count events with mperf stat](guide/stat.md)
- [Discover PMU events with mperf list](guide/list.md)
- [Record a profile with mperf record](guide/record.md)
- [Scenarios](guide/scenarios.md)
  - [Hotspots and top-down (tma)](guide/scenario-hotspots.md)
  - [System snapshot](guide/scenario-snapshot.md)
  - [Memory analysis (mem)](guide/scenario-mem.md)
  - [Roofline](guide/scenario-roofline.md)
- [Call stacks and symbols](guide/callstacks.md)
- [View results in the terminal](guide/show.md)
- [View results in the GUI](guide/gui.md)
- [Query results with SQL](guide/query.md)
- [Trace your code and runtimes](guide/tracing.md)

# Concepts

- [How miniperf measures](concepts/measurement.md)
- [Top-down microarchitecture analysis](concepts/topdown.md)
- [Roofline analysis](concepts/roofline.md)

# Platform notes

- [Linux on x86-64](platforms/x86.md)
- [Linux on Arm](platforms/arm.md)
- [Linux on RISC-V](platforms/riscv.md)
- [macOS and Windows](platforms/desktop.md)

# Tutorials

- [Diagnose a bottleneck with top-down](tutorials/topdown.md)
- [Roofline analysis of a sparse matrix kernel](tutorials/roofline.md)
- [Measure a working set](tutorials/memory.md)
- [Snapshot a busy machine](tutorials/snapshot.md)
- [Instrument an application with trace spans](tutorials/tracing.md)
- [Answer questions with SQL](tutorials/query.md)

# Reference

- [Command-line reference](reference/cli.md)
- [Environment variables](reference/environment.md)
- [Recording directory layout](reference/output.md)
- [Database schema](reference/schema.md)
- [Glossary](reference/glossary.md)
