#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static volatile uint64_t sink;

__attribute__((noinline)) void duty_60(uint64_t iterations) {
    uint64_t value = sink;
    for (uint64_t i = 0; i < iterations; ++i) {
        value = value * UINT64_C(6364136223846793005) + i + UINT64_C(1);
    }
    sink = value;
}

__attribute__((noinline)) void duty_40(uint64_t iterations) {
    uint64_t value = sink;
    for (uint64_t i = 0; i < iterations; ++i) {
        // A distinct immediate prevents identical-code folding with duty_60;
        // the generated loop still has the same instruction count per iteration.
        value = value * UINT64_C(6364136223846793005) + i + UINT64_C(3);
    }
    sink = value;
}

static double monotonic_seconds(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        perror("clock_gettime");
        exit(2);
    }
    return (double)now.tv_sec + (double)now.tv_nsec / 1000000000.0;
}

int main(int argc, char **argv) {
    const double duration = argc > 1 ? strtod(argv[1], NULL) : 3.0;
    const double deadline = monotonic_seconds() + duration;

    do {
        duty_60(UINT64_C(600000));
        duty_40(UINT64_C(400000));
    } while (monotonic_seconds() < deadline);

    return sink == UINT64_MAX;
}
