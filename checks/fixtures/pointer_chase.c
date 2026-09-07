#include <stdint.h>
#include <stdlib.h>

/* TMA acceptance fixture: serial dependent loads should have a dominant
 * backend/memory signal.  The volatile sink prevents dead-code elimination. */
int main(void) {
    enum { N = 1 << 20, ITERS = 1 << 26 };
    uint32_t *next = malloc((size_t)N * sizeof(*next));
    if (!next) return 2;
    for (uint32_t i = 0; i < N; ++i) next[i] = (i * 8191u + 17u) & (N - 1);
    volatile uint32_t index = 1;
    for (uint32_t i = 0; i < ITERS; ++i) index = next[index];
    free(next);
    return index == 0xffffffffu;
}
