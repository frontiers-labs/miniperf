// Drives the ITT shim the way Intel's ittnotify loader does: dlopen the
// library named by INTEL_LIBITTNOTIFY64 and resolve the API by symbol.
//
// argv[1] is the shim path. Emits a fixed number of task begin/end pairs so
// the recorded count is exact rather than merely non-zero.
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>

#define TASKS 100

typedef struct {
  uint64_t d1, d2, d3;
} itt_id;

typedef void *(*domain_create_fn)(const char *);
typedef void *(*string_create_fn)(const char *);
typedef void (*task_begin_fn)(const void *, itt_id, itt_id, void *);
typedef void (*task_end_fn)(const void *);

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: itt_client <libmperf_itt.so>\n");
    return 2;
  }
  void *library = dlopen(argv[1], RTLD_NOW);
  if (!library) {
    fprintf(stderr, "dlopen failed: %s\n", dlerror());
    return 1;
  }

  domain_create_fn domain_create = (domain_create_fn)dlsym(library, "__itt_domain_create");
  string_create_fn string_create = (string_create_fn)dlsym(library, "__itt_string_handle_create");
  task_begin_fn task_begin = (task_begin_fn)dlsym(library, "__itt_task_begin");
  task_end_fn task_end = (task_end_fn)dlsym(library, "__itt_task_end");
  if (!domain_create || !string_create || !task_begin || !task_end) {
    fprintf(stderr, "the shim is missing part of the ITT API\n");
    return 1;
  }

  void *domain = domain_create("mperf.check");
  void *name = string_create("flow_node");
  if (!domain || !name) {
    fprintf(stderr, "the shim returned no domain or string handle\n");
    return 1;
  }

  itt_id null_id = {0, 0, 0};
  for (int i = 0; i < TASKS; i++) {
    task_begin(domain, null_id, null_id, name);
    task_end(domain);
  }

  void (*shutdown)(void) = (void (*)(void))dlsym(library, "mperf_trace_shutdown");
  if (shutdown)
    shutdown();
  return 0;
}
