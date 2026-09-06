# Call stacks and symbols

Every sample in a recording carries a call stack. This chapter explains how miniperf gets them and how it turns addresses into names.

## How stacks are captured

miniperf tries three methods, from cheapest to most general, and picks the best one the host supports. Opening the perf event is the probe. There is no flag to set.

1. **Branch records.** On Intel CPUs with call-stack mode in the Last Branch Record buffer, each sample carries the hardware's record of recent calls. Nothing is copied from the user stack. AMD Zen 3 and newer have a branch buffer too, but it records all branches. miniperf replays that history, pushing on calls and popping on returns, to rebuild the stack.
2. **DWARF unwinding.** When branch records are unavailable, each sample copies the user registers and up to 8 KiB of stack. After the run, miniperf unwinds those copies using the binaries' unwind tables. This works for optimized binaries without frame pointers. The cost is up to 8 KiB of buffer traffic per sample. The `snapshot` scenario copies 2 KiB to keep its footprint small.
3. **Kernel callchain.** The kernel's frame-pointer walk is recorded in every mode, and miniperf uses whichever of the kernel chain and the DWARF result is deeper. Binaries built with `-fno-omit-frame-pointer` get good stacks from this alone.

Kernel frames appear only when `kernel.perf_event_paranoid` is 1 or lower and `kernel.kptr_restrict` is 0. Otherwise the stack stops at the syscall boundary.

## How addresses become names

Postprocessing resolves each address through the shared `miniperf-symbolize` library. It expands inlined frames from DWARF, so a sample inside an inlined function shows both the inlined function and its caller.

Debug information is searched in this order:

1. `/tmp/perf-<pid>.map`, the convention JIT compilers use to publish symbols.
2. A file named by the object's `.gnu_debuglink`, next to the object, in `.debug/`, or under `/usr/lib/debug`.
3. The miniperf build-id cache, `~/.cache/miniperf/buildid/<build-id>/debuginfo`.
4. The system build-id tree, `/usr/lib/debug/.build-id`.
5. The mapped object itself.

Addresses that resolve to nothing appear as `[unknown]`.

The cache root follows `MINIPERF_CACHE_DIR`, then `XDG_CACHE_HOME`, then `HOME`.

## Fetch debug info from debuginfod

Symbolization is offline by default. To let it fetch debug files from a debuginfod server, set both variables:

```sh
export DEBUGINFOD_URLS=https://debuginfod.archlinux.org
export MINIPERF_DEBUGINFOD=1
```

miniperf calls the installed `debuginfod-find` tool for each build id it cannot resolve locally and copies successful downloads into its cache. If the tool is missing or the server is unreachable, symbolization falls back to local files. `mperf doctor` warns when `DEBUGINFOD_URLS` is set but the tool is not installed.

## Assembly views

The assembly tabs in `mperf show` and `mperf-gui` need `objdump` on `PATH`. Without it the recording still works and postprocessing prints `skipping assembly extraction`.
