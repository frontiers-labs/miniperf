#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

enum { SLEEP_MILLISECONDS = 250 };

static int pipe_fds[2];

__attribute__((noinline)) static void *blocked_on_truth_pipe(void *unused) {
    (void)unused;
    uint8_t byte;
    while (read(pipe_fds[0], &byte, sizeof(byte)) < 0 && errno == EINTR) {
    }
    return NULL;
}

int main(void) {
    pthread_t sleeper;
    const struct timespec delay = {
        .tv_sec = 0,
        .tv_nsec = SLEEP_MILLISECONDS * 1000000L,
    };
    const uint8_t byte = 1;

    if (pipe(pipe_fds) != 0 ||
        pthread_create(&sleeper, NULL, blocked_on_truth_pipe, NULL) != 0) {
        return 2;
    }
    if (nanosleep(&delay, NULL) != 0 || write(pipe_fds[1], &byte, sizeof(byte)) != 1) {
        return 3;
    }
    return pthread_join(sleeper, NULL) == 0 ? 0 : 4;
}
