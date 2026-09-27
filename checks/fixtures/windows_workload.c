#include <stdint.h>
#include <windows.h>

static volatile uint32_t values[4096];
static volatile double result;

int main(void) {
    uint32_t state = 1;
    const ULONGLONG finish = GetTickCount64() + 3000;
    while (GetTickCount64() < finish) {
        state = state * 1664525u + 1013904223u;
        const uint32_t index = state & 4095u;
        values[index] += state;
        result += (double)values[index] * 0.000001;
    }
    return result < 0.0;
}
