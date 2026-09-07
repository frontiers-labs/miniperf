// Is a hardware cycles counter usable on this host? Exits 0 when yes.
//
// Deliberately links nothing from miniperf. CI reruns a job that reports no
// PMU, and if this used the profiler's own capability probing, a regression in
// that probing would be indistinguishable from an Intel placement and would be
// retried into a green build.
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifdef __linux__
#include <linux/perf_event.h>

static volatile uint64_t sink;

int main(void) {
  struct perf_event_attr attr;
  memset(&attr, 0, sizeof(attr));
  attr.type = PERF_TYPE_HARDWARE;
  attr.size = sizeof(attr);
  attr.config = PERF_COUNT_HW_CPU_CYCLES;
  attr.disabled = 1;
  attr.exclude_kernel = 1;
  attr.exclude_hv = 1;

  long fd = syscall(SYS_perf_event_open, &attr, 0, -1, -1, 0);
  if (fd < 0) {
    printf("pmu_oracle: absent (perf_event_open: %s)\n", strerror(errno));
    return 1;
  }
  ioctl(fd, PERF_EVENT_IOC_RESET, 0);
  ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
  uint64_t acc = 1;
  for (uint64_t i = 0; i < 10000000ull; i++)
    acc = acc * 6364136223846793005ull + i;
  sink = acc;
  ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);

  uint64_t cycles = 0;
  if (read(fd, &cycles, sizeof(cycles)) != (ssize_t)sizeof(cycles)) {
    printf("pmu_oracle: absent (counter unreadable)\n");
    close(fd);
    return 1;
  }
  close(fd);
  if (cycles == 0) {
    printf("pmu_oracle: absent (counter opened but never moved)\n");
    return 1;
  }
  printf("pmu_oracle: present (%llu cycles)\n", (unsigned long long)cycles);
  return 0;
}
#else
int main(void) {
  printf("pmu_oracle: absent (no perf_event_open on this platform)\n");
  return 1;
}
#endif
