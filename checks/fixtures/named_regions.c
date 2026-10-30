#define _POSIX_C_SOURCE 200809L
#include <mperf_trace.h>
#include <pthread.h>
#include <stdint.h>
#include <time.h>

static uint64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000000000 + (uint64_t)ts.tv_nsec;
}

static void busy(void) {
  uint64_t end = now_ns() + 20000000;
  volatile uint64_t value = 1;
  while (now_ns() < end)
    value = value * 6364136223846793005ull + 1;
}

static void recurse(int depth) {
  MPERF_TRACE_POINT(point, "recursive", 0);
  uint64_t instance = mperf_trace_begin(point, 0);
  busy();
  if (depth)
    recurse(depth - 1);
  mperf_trace_end(point, instance);
}

static void *worker(void *unused) {
  (void)unused;
  MPERF_TRACE_POINT(point, "worker", 0);
  uint64_t instance = mperf_trace_begin(point, 0);
  recurse(1);
  mperf_trace_end(point, instance);
  return NULL;
}

int main(void) {
  MPERF_TRACE_POINT(parent, "iteration", 0);
  MPERF_TRACE_POINT(child, "compute", 0);
  MPERF_TRACE_POINT(other, "other_parent", 0);
  MPERF_TRACE_POINT(incomplete, "unfinished", 0);
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  for (int i = 0; i < 4; ++i) {
    uint64_t outer = mperf_trace_begin(parent, 0);
    busy();
    uint64_t inner = mperf_trace_begin(child, i % 2 ? outer : 0);
    busy();
    mperf_trace_end(child, inner);
    mperf_trace_end(parent, outer);
    busy();
  }
  uint64_t outer = mperf_trace_begin(other, 0);
  uint64_t inner = mperf_trace_begin(child, outer);
  busy();
  mperf_trace_end(child, inner);
  mperf_trace_end(other, outer);
  pthread_join(thread, NULL);
  mperf_trace_begin(incomplete, 0);
  busy();
  mperf_trace_shutdown();
  return 0;
}
