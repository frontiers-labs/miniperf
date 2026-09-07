// Plays the part of the CUDA runtime for the CUPTI shim: dlopen the library
// named by CUDA_INJECTION64_PATH and call InitializeInjection, which is the
// whole contract. argv[1] is the shim path.
#include <dlfcn.h>
#include <stdio.h>

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: cupti_inject <libmperf_cupti.so>\n");
    return 2;
  }
  void *library = dlopen(argv[1], RTLD_NOW);
  if (!library) {
    fprintf(stderr, "dlopen failed: %s\n", dlerror());
    return 1;
  }
  int (*initialize)(void) = (int (*)(void))dlsym(library, "InitializeInjection");
  if (!initialize) {
    fprintf(stderr, "InitializeInjection is missing\n");
    return 1;
  }
  printf("InitializeInjection returned %d\n", initialize());
  return 0;
}
