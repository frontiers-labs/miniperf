// Two OpenMP parallel regions with a known thread count, so the OMPT shim's
// recorded region and thread counts are exact.
#include <stdint.h>
#include <stdio.h>

volatile uint64_t sink;

static void burn(void) {
  uint64_t acc = 1;
  for (int i = 0; i < 200000; i++)
    acc = acc * 6364136223846793005ull + (uint64_t)i;
  sink = acc;
}

int main(void) {
#pragma omp parallel
  burn();
#pragma omp parallel
  burn();
  printf("done\n");
  return 0;
}
