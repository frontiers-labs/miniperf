// Allocation traffic for the libc shim check.
//
// The block is written to and the result escapes through a volatile sink:
// at -O2 a malloc/free pair whose result is unused is removed outright, and
// the shim then has nothing to interpose on.
#include <stdlib.h>

volatile unsigned char sink;

int main(void) {
  for (int i = 0; i < 200000; i++) {
    size_t size = 64 + (size_t)(i % 512);
    unsigned char *block = malloc(size);
    if (!block)
      return 1;
    block[0] = (unsigned char)i;
    block[size - 1] = (unsigned char)(i >> 8);
    sink = block[0] ^ block[size - 1];
    free(block);
  }
  return 0;
}
