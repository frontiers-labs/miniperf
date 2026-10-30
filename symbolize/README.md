# miniperf-symbolize

Shared native symbolization for `mperf` and the optional `libprof`
`symbolize` feature.

Resolution order on Linux is:

1. `/tmp/perf-<pid>.map` for JIT-generated code.
2. A valid `.gnu_debuglink` file beside the object, in `.debug/`, or beneath
   `/usr/lib/debug`.
3. The miniperf build-id cache at
   `~/.cache/miniperf/buildid/<hex-build-id>/debuginfo`.
4. The system build-id tree at `/usr/lib/debug/.build-id`.
5. The mapped object itself.

`MINIPERF_CACHE_DIR` overrides the cache root. `XDG_CACHE_HOME` is honored when
the miniperf-specific override is absent.

Network lookup is off by default. When both `MINIPERF_DEBUGINFOD=1` and
`DEBUGINFOD_URLS` are set, the resolver invokes an installed
`debuginfod-find debuginfo <build-id>` client and copies a successful result
into the miniperf cache. The crate contains no HTTP client, does not access the
network during builds or tests, and degrades to local symbols when the command
is unavailable or a server cannot be reached.

`FunctionName` keeps its complete input and exposes a separate friendly label.
Rust hashes and v0 crate disambiguators identify already-demangled Rust input;
unrecognized names pass through unchanged. Generic arguments become `<…>`,
while receiver and trait paths remain visible. Callers must retain the complete
name or original linkage name for identity, search, and assembly lookup.
`demangle_full` retains Rust hashes that addr2line's alternate formatter omits.
Resolved frames also expose the original linkage name when available.
Folded files mark percent-encoded name delimiters with a version comment. The
terminal renderer decodes names after parsing the stack, so Rust array types
such as `[u64; 4]` do not introduce extra frames. Headerless files retain their
previous interpretation. External folded-stack consumers must decode each
frame after splitting the stack when this version comment is present. Exported
SVGs retain complete encoded names, so delimiter escapes such as `%3B` remain
visible there; friendly labels apply to the terminal and desktop viewers.

Run the compiled legacy/v0 fixture and recording check with an unpacked package:

```sh
MPERF_PACKAGE=/path/to/package cargo xtask check rust-names
```

The check compiles real Rust code, resolves its symbol and inline ranges, and
records each executable with the packaged `mperf`. It checks monomorphization
identity and source locations through `mperf query`. The fixture compiler alone
uses `RUSTC_BOOTSTRAP=1 -Z unstable-options`, because newer stable Rust versions
gate legacy symbol emission. No profiler build uses that setting. Without PMU
access the object checks still run and the recording check reports a skip.
`cargo xtask check gui` then visits the recordings with the packaged viewer,
checks searches using both spellings, and opens individual source/assembly tabs.
