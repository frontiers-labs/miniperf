# Record a profile with mperf record

`mperf record` samples a program and writes a result directory that `mperf show`, `mperf-gui`, and `mperf query` read. Every recording names a scenario and an output directory:

```sh
mperf record -s tma -o results/run-1 -- ./workload --input data.bin
```

The scenario decides what is collected. This chapter covers what all scenarios share. The [Scenarios](scenarios.md) chapter helps you choose one.

## What happens during a recording

1. `mperf` resolves the best available hardware mechanism for the scenario and prints it:

   ```
   Record profile with TMA scenario
   Capture fidelity: tma at 'counter_only' (fixed topdown: core PMU exposes no `slots` + `topdown-*` events (PERF_METRICS is Icelake and newer))
   ```

   The parenthesis explains why the first-choice mechanism was rejected. The same information lands in the `capture_fidelity` table.

2. For `roofline` and `mem`, it calibrates the host's compute and memory ceilings. This takes a few seconds and prints the result.

3. It starts the workload with sampling attached, at 1000 samples per second per CPU, or 99 for `snapshot`. Optional collectors that cannot run print a warning and are recorded as unavailable:

   ```
   Warning: precise_memory: PEBS: core PMU advertises max_precise=0 — precise sampling needs 2
   ```

4. When the workload exits, it symbolizes the samples, builds the analysis tables, and renders flame graphs:

   ```
   Postprocessing...
   ```

The workload's own stdout and stderr pass through untouched.

## The output directory

The directory must not exist. `mperf record` refuses to write into an existing directory so that two runs never mix:

```
Error: profiling results must be put in different directories

Caused by:
    'results/run-1' already exists
```

Inside you find `info.json` with the recording metadata, one Parquet file per table, and the flame graph SVGs. [Recording directory layout](../reference/output.md) lists every file. The directory is self-contained. Copy it to another machine and open it there.

## Attach to a running process

The `snapshot` scenario can attach to an existing process tree instead of launching one:

```sh
sudo mperf record -s snapshot -o results/server -p 4242 --duration 30s
```

The other scenarios need to launch the program, because they measure it from the first instruction or replay it under an instrumentation engine.

## Bound the recording time

`--duration` stops a snapshot after a fixed time. It accepts `ms`, `s`, `m`, and `h` suffixes, and a bare number means seconds:

```sh
mperf record -s snapshot -o results/burst --duration 10s -- ./server
```

When the time expires on a launched command, `mperf` sends `SIGTERM` to the process tree, waits two seconds, then sends `SIGKILL`. Other scenarios ignore `--duration` and run until the program exits.

## Pin the CPUs

Wrap `mperf` itself in `taskset`, not only the workload:

```sh
taskset -c 0-3 mperf record -s roofline -o results/spmv -- ./spmv
```

Pinning matters for three reasons. Roofline calibration runs on the same CPUs as the workload, so the ceilings match. Sampling opens one buffer per CPU, so fewer CPUs fit within the kernel's memory budget (see [Set up permissions](permissions.md)). And the run is repeatable.

## Get useful stacks

Build the workload with `-g`. For programs compiled without frame pointers, miniperf captures registers and up to 8 KiB of stack per sample and unwinds them with DWARF after the run. On Intel CPUs with call-stack LBR it records branch records instead, which is cheaper. Neither needs a flag. See [Call stacks and symbols](callstacks.md).

## Recover an interrupted recording

If `mperf record` is killed mid-run, the directory holds Parquet segments without footers. `mperf recover` validates the directory and quarantines the damaged segments so the rest opens:

```sh
mperf recover results/run-1
```

```
7 healthy segment(s)
quarantined results/run-1/samples_raw-4242-3.parquet
```

Postprocessing does not rerun, so tables that are built after the workload exits are missing from a recovered recording. The raw samples remain queryable.
