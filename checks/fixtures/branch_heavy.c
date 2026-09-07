#include <stdint.h>

/* TMA acceptance fixture: a data-dependent, pseudo-random branch stream
 * should expose bad-speculation above a straight-line control fixture. */
int main(void) {
    uint32_t state = 1;
    volatile uint32_t sum = 0;
    for (uint32_t i = 0; i < (1u << 28); ++i) {
        state = state * 1664525u + 1013904223u;
        if (state & 0x80000000u) sum += i; else sum -= i;
    }
    return sum == 0xffffffffu;
}
