# Allocation lifetimes

```sh
mperf record -s snapshot --allocations sampled -o results/heap -- ./app
```

The same `--allocations` option works with `tma`, `mem`, and `roofline`.
Snapshot runs the application once. Multi-pass backends capture allocations in
the native performance pass. `mem` defaults to `metadata`; other scenarios
default to `off`. Explicit generic trace points work independently.

| Mode | Lifetime boundaries | Stack attribution |
|---|---|---|
| `off` | Disabled | Disabled |
| `metadata` | All observed operations | Disabled |
| `sampled` | All observed operations | Every sixteenth successful introduction per thread |
| `full` | All observed operations | Every successful introduction |

On Linux/glibc, capture covers libc-interposable malloc, calloc, realloc, aligned_alloc,
posix_memalign and free in the launched root process. Fork descendants, static
binaries, protected executables and custom allocators are outside that scope.
Attach reports allocation capture unavailable. The source preserves the caller's
`LD_PRELOAD`. Stack capture uses fixed storage and unwind information, including
optimized programs without frame pointers. Missing debug or unwind information
can leave partial or unknown attribution.

The Allocations tab loads independently of the memory-reference analysis. It
shows count, requested bytes, live bytes, observed peak and ended lifetimes per
call stack. Select a row to inspect its stack and select a frame to open its full
function name and source. The table selects allocations born in the chosen time
range and named region, then applies thread, module and symbol filters. Live
bytes are that cohort's live bytes at the selection end. Whole-recording totals
and the chart remain labeled separately.

Requested bytes measure successful application requests, not allocator usable
size or RSS. Live at recording end is not evidence of a leak. Realloc introduces
a new generation after success and retires the old generation at the pre-call
boundary. Nonzero failure preserves the old generation. On glibc, realloc(p,0)
retires p even when it returns NULL. Internal live bytes during each realloc
interval are uncertain; observed peaks do not claim the allocator's internal
peak. Requested bytes sum the full size of every successful introduction,
including successful realloc results. Realloc growth sums positive differences
from the matched old requested size.

`allocation_capture` records the graceful footer, producer losses, lost IPC
operations and packets, pauses and protocol status. Missing footers and any
loss make the capture incomplete. Without a footer, cumulative producer and
operation-loss counters are lower bounds from the last received batch. Unknown-stack allocations remain visible and
sampled stack counts are not extrapolated. `allocation_operations.stack_status`
is 0 when attribution was not selected, 1 when raw frames were captured, 2 when
no frames were available, 3 at the frame limit, and 4 on an unwind error. Captured
frames remain best effort. Losses also prevent precise-memory samples from being
joined to uncertain allocation ranges.

The SQL tables expose raw ownership boundaries, generations and stacks:

```sh
mperf query results/heap 'SELECT * FROM allocation_capture'
mperf query results/heap 'SELECT * FROM allocation_summary'
mperf query results/heap 'SELECT * FROM allocation_sites ORDER BY requested_bytes DESC'
mperf query results/heap 'SELECT * FROM allocation_stacks WHERE stack_id = 1 ORDER BY frame_index, inline_index'
```

Allocation identity is `(pid, process_epoch, allocation_id)`. Addresses and
function names are not identities. Stacks are interned by full raw-frame equality
within the recording. Full names and linkage names remain in the stack table;
friendly names only change display.

For a heap snapshot at time `T`, query half-open ownership intervals:

```sql
SELECT pid, process_epoch, allocation_id, address, requested_bytes, stack_id
FROM allocations
WHERE allocated_ns <= T AND (freed_ns IS NULL OR T < freed_ns);
```

Inspect `allocation_realloc_intervals` as well when T falls inside a realloc
call. `allocation_timeline` reports received-operation live bytes. Treat totals,
peaks and lifetime endings as observations when `allocation_summary.complete=0`.
