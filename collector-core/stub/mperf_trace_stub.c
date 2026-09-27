/* Static no-op stub for libmperf-trace. Linked directly into applications:
 * if MPERF_SESSION_DIR is set it dlopens the collector core and forwards,
 * otherwise every call costs one predictable branch. */
#include "../include/mperf_trace.h"

#include <stdlib.h>
#ifdef _WIN32
#include <windows.h>
#define MPERF_SYMBOL(module, name) GetProcAddress((HMODULE)(module), (name))
static void *mperf_open_library(const char *name) {
    return (void *)LoadLibraryA(name ? name : "mperf_collector.dll");
}
#else
#include <dlfcn.h>
#define MPERF_SYMBOL(module, name) dlsym((module), (name))
static void *mperf_open_library(const char *name) {
    return dlopen(name ? name : "libmperf_collector.so", RTLD_NOW | RTLD_GLOBAL);
}
#endif

typedef struct {
    mperf_trace_handle_t *(*reg)(const mperf_trace_payload_t *);
    uint64_t (*begin)(mperf_trace_handle_t *, uint64_t);
    void (*end)(mperf_trace_handle_t *, uint64_t);
    void (*instant)(mperf_trace_handle_t *, int64_t);
    void (*counter)(mperf_trace_handle_t *, int64_t);
    void (*shutdown)(void);
} mperf_vtable_t;

static mperf_vtable_t mperf_vtable;
static int mperf_state; /* 0 = unresolved, 1 = active, -1 = disabled */

static int mperf_resolve(void) {
    if (mperf_state)
        return mperf_state;
    if (!getenv("MPERF_SESSION_DIR")) {
        mperf_state = -1;
        return mperf_state;
    }
    const char *library = getenv("MPERF_COLLECTOR_LIBRARY");
    void *core = mperf_open_library(library);
    if (!core) {
        mperf_state = -1;
        return mperf_state;
    }
    mperf_vtable.reg = (mperf_trace_handle_t * (*)(const mperf_trace_payload_t *))
        MPERF_SYMBOL(core, "mperf_trace_register");
    mperf_vtable.begin = (uint64_t (*)(mperf_trace_handle_t *, uint64_t))MPERF_SYMBOL(
        core, "mperf_trace_begin");
    mperf_vtable.end = (void (*)(mperf_trace_handle_t *, uint64_t))MPERF_SYMBOL(
        core, "mperf_trace_end");
    mperf_vtable.instant = (void (*)(mperf_trace_handle_t *, int64_t))MPERF_SYMBOL(
        core, "mperf_trace_instant");
    mperf_vtable.counter = (void (*)(mperf_trace_handle_t *, int64_t))MPERF_SYMBOL(
        core, "mperf_trace_counter");
    mperf_vtable.shutdown = (void (*)(void))MPERF_SYMBOL(core, "mperf_trace_shutdown");
    mperf_state = mperf_vtable.reg ? 1 : -1;
    return mperf_state;
}

mperf_trace_handle_t *mperf_trace_register(const mperf_trace_payload_t *payload) {
    if (mperf_resolve() != 1)
        return 0;
    return mperf_vtable.reg(payload);
}

uint64_t mperf_trace_begin(mperf_trace_handle_t *handle, uint64_t parent) {
    if (!handle || mperf_state != 1)
        return 0;
    return mperf_vtable.begin(handle, parent);
}

void mperf_trace_end(mperf_trace_handle_t *handle, uint64_t instance) {
    if (handle && mperf_state == 1)
        mperf_vtable.end(handle, instance);
}

void mperf_trace_instant(mperf_trace_handle_t *handle, int64_t value) {
    if (handle && mperf_state == 1)
        mperf_vtable.instant(handle, value);
}

void mperf_trace_counter(mperf_trace_handle_t *handle, int64_t value) {
    if (handle && mperf_state == 1)
        mperf_vtable.counter(handle, value);
}

void mperf_trace_shutdown(void) {
    if (mperf_state == 1)
        mperf_vtable.shutdown();
}

/* Roofline instrumentation entry points emitted by the miniperf Clang pass.
 * Same pattern: no-ops unless the collector core is loadable, so linking this
 * stub never makes the collector a runtime dependency. */

typedef struct mperf_roofline_handle mperf_roofline_handle_t;

typedef struct {
    mperf_roofline_handle_t *(*begin)(const void *);
    void (*end)(mperf_roofline_handle_t *);
    void (*stats)(mperf_roofline_handle_t *, const void *);
    int (*is_instrumented)(void);
} mperf_roofline_vtable_t;

static mperf_roofline_vtable_t mperf_roofline_vtable;
static int mperf_roofline_state;

static int mperf_roofline_resolve(void) {
    if (mperf_roofline_state)
        return mperf_roofline_state;
    if (mperf_resolve() != 1) {
        mperf_roofline_state = -1;
        return mperf_roofline_state;
    }
    const char *library = getenv("MPERF_COLLECTOR_LIBRARY");
    void *core = mperf_open_library(library);
    if (!core) {
        mperf_roofline_state = -1;
        return mperf_roofline_state;
    }
    mperf_roofline_vtable.begin = (mperf_roofline_handle_t * (*)(const void *))
        MPERF_SYMBOL(core, "mperf_roofline_internal_notify_loop_begin");
    mperf_roofline_vtable.end = (void (*)(mperf_roofline_handle_t *))MPERF_SYMBOL(
        core, "mperf_roofline_internal_notify_loop_end");
    mperf_roofline_vtable.stats =
        (void (*)(mperf_roofline_handle_t *, const void *))MPERF_SYMBOL(
            core, "mperf_roofline_internal_notify_loop_stats");
    mperf_roofline_vtable.is_instrumented = (int (*)(void))MPERF_SYMBOL(
        core, "mperf_roofline_internal_is_instrumented_profiling");
    mperf_roofline_state = mperf_roofline_vtable.begin ? 1 : -1;
    return mperf_roofline_state;
}

mperf_roofline_handle_t *
mperf_roofline_internal_notify_loop_begin(const void *info) {
    if (mperf_roofline_resolve() != 1)
        return 0;
    return mperf_roofline_vtable.begin(info);
}

void mperf_roofline_internal_notify_loop_end(mperf_roofline_handle_t *handle) {
    if (handle && mperf_roofline_state == 1)
        mperf_roofline_vtable.end(handle);
}

void mperf_roofline_internal_notify_loop_stats(mperf_roofline_handle_t *handle,
                                               const void *stats) {
    if (handle && mperf_roofline_state == 1)
        mperf_roofline_vtable.stats(handle, stats);
}

int mperf_roofline_internal_is_instrumented_profiling(void) {
    if (mperf_roofline_resolve() != 1)
        return 0;
    return mperf_roofline_vtable.is_instrumented
               ? mperf_roofline_vtable.is_instrumented()
               : 0;
}
