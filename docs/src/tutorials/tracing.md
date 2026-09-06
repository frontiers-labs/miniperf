# Instrument an application with trace spans

In this tutorial we add spans, a counter, and an instant marker to a C program, record it, and read the trace next to the sampled profile. Spans answer a question samples cannot: how long did this phase take, and how many times did it run.

## Add trace points

Start from `matmul.c` in the [top-down tutorial](topdown.md). Include the header, wrap the phases in scopes, and emit a counter per iteration:

```c
#include <mperf_trace.h>

static void fill(void) {
    MPERF_SCOPE("fill");
    /* ... unchanged ... */
}

static void multiply(void) {
    MPERF_SCOPE("multiply");
    /* ... unchanged ... */
}

int main(int argc, char **argv) {
    int reps = argc > 1 ? atoi(argv[1]) : 3;
    fill();
    for (int r = 0; r < reps; r++) {
        multiply();
        MPERF_COUNTER("iteration", r);
    }
    MPERF_INSTANT("done", reps);
    printf("checksum %.3f\n", checksum());
    return 0;
}
```

`MPERF_SCOPE` is a C++ construct: it declares an object whose destructor ends the span. Compile the file as C++, and compile the stub as C:

```sh
g++ -x c++ -O2 -g -I miniperf/collector-core/include matmul.c \
    -x c miniperf/collector-core/stub/mperf_trace_stub.c -o traced -ldl
./traced 1
```

The program runs normally. Every trace call is a no-op when no collector is present, so the instrumented binary is safe to ship.

In plain C, use `mperf_trace_register` once per site and `mperf_trace_begin` and `mperf_trace_end` around the phase. The [tracing chapter](../guide/tracing.md) lists the functions.

## Record

```sh
mperf record -s snapshot -o traced-out -- ./traced 3
```

If you built miniperf from source, tell the stub where the collector library is:

```sh
MPERF_COLLECTOR_LIBRARY=target/release/libmperf_collector.so \
  mperf record -s snapshot -o traced-out -- ./traced 3
```

## Read the trace

```sh
mperf query traced-out 'SELECT name, kind, count, total_ns, total_value FROM custom_events'
```

```
┌───────────┬─────────┬───────┬───────────────┬─────────────┐
│    name   ┆   kind  ┆ count ┆    total_ns   ┆ total_value │
╞═══════════╪═════════╪═══════╪═══════════════╪═════════════╡
│ multiply  ┆ span    ┆     3 ┆ 1,141,430,200 ┆           0 │
│ fill      ┆ span    ┆     1 ┆     2,156,406 ┆           0 │
│ iteration ┆ counter ┆     3 ┆             0 ┆           3 │
│ done      ┆ instant ┆     1 ┆             0 ┆           3 │
└───────────┴─────────┴───────┴───────────────┴─────────────┘
```

Three multiplies took 1.14 seconds together, and `fill` took 2.2 milliseconds. The counter's `total_value` is the sum of the values passed, 0 + 1 + 2. The instant carries its value, 3.

## Read individual events

The aggregate hides the timeline. The raw events are in the `events` table, with names one join away:

```sh
mperf query traced-out '
  SELECT e.timestamp, e.type, e.tid, s.string AS name
  FROM events e
  JOIN payloads p ON p.event_id = e.event_id
  JOIN strings s ON s.id = p.name_id
  ORDER BY e.timestamp'
```

Type 0 is a span begin, 1 its end, 2 an instant, 3 a counter. An end row's `flow_id` equals its begin row's `instance`, which is how the aggregate pairs them. Subtracting consecutive begin timestamps of `multiply` gives per-iteration durations.

## Combine with samples

The trace and the samples are in the same recording with the same clock. To attribute samples to a phase, join on time:

```sh
mperf query traced-out '
  WITH spans AS (
    SELECT b.timestamp AS start_ns, e.timestamp AS end_ns
    FROM events b JOIN events e ON e.flow_id = b.instance AND e.type = 1
    JOIN payloads p ON p.event_id = b.event_id
    JOIN strings s ON s.id = p.name_id
    WHERE b.type = 0 AND s.string = '"'"'multiply'"'"')
  SELECT COUNT(*) AS samples_in_multiply,
         (SELECT COUNT(*) FROM pmu_counters) AS all_samples
  FROM pmu_counters c JOIN spans ON c.timestamp BETWEEN spans.start_ns AND spans.end_ns'
```

```
┌─────────────────────┬─────────────┐
│ samples_in_multiply ┆ all_samples │
╞═════════════════════╪═════════════╡
│                 123 ┆         144 │
└─────────────────────┴─────────────┘
```

123 of 144 samples landed inside `multiply` spans, at the snapshot rate of 99 Hz. The rest fell in `fill`, program startup, and the checksum.

## Watch out for

- **Optimized-away work.** If `c` is never read, the compiler deletes the multiply and the span reports a few hundred nanoseconds. The trace is honest; the program did nothing.
- **Dropped events.** Buffers are bounded. A thread that emits faster than the writer drains gets `loss` rows in `custom_events` with the dropped count. Emit fewer events, or move counters out of the innermost loop.
- **Stacks on trace points.** The macros do not capture stacks. Register a payload with `MPERF_TRACE_FLAG_STACK` if you need to know who called a traced function.
