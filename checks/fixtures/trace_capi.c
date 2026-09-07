// Exercises the manual tracing API exactly as docs/src/guide/tracing.md tells
// users to: include the shipped header, compile the shipped stub, and let the
// calls resolve to the collector at runtime.
#include <mperf_trace.h>
#include <stdint.h>

volatile uint64_t sink;

int main(void) {
  MPERF_TRACE_POINT(span, "check_span", 0);
  for (int i = 0; i < 64; i++) {
    uint64_t instance = mperf_trace_begin(span, 0);
    uint64_t acc = 1;
    for (int j = 0; j < 20000; j++)
      acc = acc * 6364136223846793005ull + (uint64_t)j;
    sink = acc;
    mperf_trace_end(span, instance);
    MPERF_COUNTER("check_counter", i);
    MPERF_INSTANT("check_instant", i);
  }
  mperf_trace_shutdown();
  return 0;
}
