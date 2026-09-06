/* Roofline smoke fixture: N worker threads each run the same daxpy loop
 * over a private buffer, so one loop is executed by every thread at once. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

enum { ELEMENTS = 1 << 15 };

static long repetitions;

static void *daxpy(void *argument) {
    double *x = malloc(ELEMENTS * sizeof *x);
    double *y = malloc(ELEMENTS * sizeof *y);
    if (x == NULL || y == NULL)
        abort();
    for (int i = 0; i < ELEMENTS; i++) {
        x[i] = i * 0.5;
        y[i] = 1.0;
    }
    double a = 1.000001;
    for (long r = 0; r < repetitions; r++) {
        for (int i = 0; i < ELEMENTS; i++)
            y[i] = a * x[i] + y[i];
        a += 1e-9;
    }
    double sum = 0.0;
    for (int i = 0; i < ELEMENTS; i++)
        sum += y[i];
    free(x);
    free(y);
    *(double *)argument = sum;
    return NULL;
}

int main(int argc, char **argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 2;
    repetitions = argc > 2 ? atol(argv[2]) : 20000;
    if (threads < 1 || repetitions < 1) {
        fprintf(stderr, "usage: %s [threads] [repetitions]\n", argv[0]);
        return 2;
    }
    pthread_t *ids = calloc(threads, sizeof *ids);
    double *sums = calloc(threads, sizeof *sums);
    if (ids == NULL || sums == NULL)
        abort();
    for (int t = 0; t < threads; t++)
        if (pthread_create(&ids[t], NULL, daxpy, &sums[t]) != 0)
            abort();
    double total = 0.0;
    for (int t = 0; t < threads; t++) {
        pthread_join(ids[t], NULL);
        total += sums[t];
    }
    printf("%d threads, checksum %g\n", threads, total);
    free(ids);
    free(sums);
    return 0;
}
