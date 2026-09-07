/* DynamoRIO client for miniperf roofline/memory accounting.
 *
 * All analysis lives in the shared roofline-core Rust staticlib (see
 * roofline_core.h); this file is the DynamoRIO instrumentation adapter. It
 * emits the same three artifacts as the QEMU plugin:
 *   <output>            key=value counters
 *   <output minus .counts>.cfg          CFG v4
 *   <output minus .counts>.memory.json  memory profile (when enabled)
 *
 * Client options (drrun -c libdr_roofline.so <options> -- app):
 *   output=<path> cache-line=<n> llc-size=<n> llc-assoc=<n>
 *   memory-profile=on|off progress=<path>
 */

#include "dr_api.h"
#include "drmgr.h"
#include "drutil.h"
#include "drreg.h"
#include "drx.h"
#include "roofline_core.h"
#include <stddef.h>
#include <string.h>
#include <stdlib.h>

#define DEFAULT_CACHE_LINE 64
#define DEFAULT_LLC_SIZE (8ULL * 1024 * 1024)
#define DEFAULT_LLC_ASSOC 16

/* Exact-address and unclassified events use a per-thread trace buffer. The
 * low-overhead x86 Roofline pass instead keeps one atomic execution counter
 * per translated block and derives architectural bytes from static operands. */
#define RECORD_BUF_SIZE (1 << 20)

static rc_session_t *session;
static uint32_t target;
static bool in_child; /* forked child: do not write artifacts */
static bool debug_unclassified;
static bool debug_classify;
static bool memory_profile;
static file_t progress_file = INVALID_FILE;

static int tls_index = -1;
static uint thread_counter;

typedef struct bb_data_t {
    uint64_t vaddr;
    uint64_t end_vaddr;
    uint32_t flow;
    uint32_t handle; /* rc_register_block handle; UINT32_MAX when invalid */
    rc_cost_t cost;
    uint64_t instructions;
    uint64_t arch_bytes_load;
    uint64_t arch_bytes_store;
    uint64_t executions;
    uint64_t successors[2];
    uint32_t successor_count;
    struct bb_data_t *next;
} bb_data_t;

typedef struct rvv_info_t {
    uint64_t block;
    uint32_t is_float;
    uint32_t masked;
    uint64_t factor;
    uint64_t sew_scale;
    struct rvv_info_t *next;
} rvv_info_t;

static void *bb_list_lock;
static bb_data_t *bb_list;
static rvv_info_t *rvv_list;
static drx_buf_t *record_buf;

static uint32_t
thread_index(void *drcontext)
{
    return (uint32_t)(uintptr_t)drmgr_get_tls_field(drcontext, tls_index);
}

/* ---------------- events fired from instrumented code ---------------- */

/* Called by drx_buf when a thread's record buffer fills up (and once more at
 * thread exit for the partial remainder). */
static void
flush_records(void *drcontext, void *buf_base, size_t size)
{
    if (in_child || session == NULL)
        return;
    rc_process_batch(session, thread_index(drcontext),
                     (const rc_record_t *)buf_base,
                     size / sizeof(rc_record_t));
}

#ifdef RISCV64
static void
rvv_event(rvv_info_t *info)
{
    if (in_child)
        return;
    void *drcontext = dr_get_current_drcontext();
    dr_mcontext_t mc = { sizeof(mc), DR_MC_ALL };
    if (!dr_get_mcontext(drcontext, &mc)) {
        rc_rvv_state_error(session);
        return;
    }
    int64_t sew = rc_rvv_sew(mc.vtype, 64);
    if (sew < 0 || info->sew_scale == 0 ||
        (uint64_t)sew > UINT64_MAX / info->sew_scale) {
        rc_rvv_state_error(session);
        return;
    }
    int64_t elements;
    if (info->masked) {
        elements = rc_active_elements(mc.vstart, mc.vl,
                                      (const uint8_t *)mc.simd[0].u64,
                                      sizeof(mc.simd[0]));
    } else {
        elements = rc_active_elements(mc.vstart, mc.vl, NULL, 0);
    }
    if (elements < 0) {
        rc_rvv_state_error(session);
        return;
    }
    rc_rvv_exec(session, info->block, info->is_float,
                (uint64_t)sew * info->sew_scale,
                (uint64_t)elements * info->factor);
}
#endif

/* ---------------- translation-time helpers ---------------- */

static void
disassemble_instr(void *drcontext, instr_t *instr, char *buffer, size_t size)
{
    buffer[0] = '\0';
    instr_disassemble_to_buffer(drcontext, instr, buffer, size);
    buffer[size - 1] = '\0';
}

static uint32_t
flow_kind(instr_t *instr)
{
    if (instr_is_return(instr))
        return RC_FLOW_RETURN;
    if (instr_is_call(instr))
        return RC_FLOW_CALL;
    return RC_FLOW_NORMAL;
}

/* ---------------- bb instrumentation ---------------- */

static dr_emit_flags_t
event_bb_app2app(void *drcontext, void *tag, instrlist_t *bb, bool for_trace,
                 bool translating)
{
    if (!drutil_expand_rep_string(drcontext, bb))
        DR_ASSERT(false);
    return DR_EMIT_DEFAULT;
}

static dr_emit_flags_t
event_bb_analysis(void *drcontext, void *tag, instrlist_t *bb, bool for_trace,
                  bool translating, void **user_data)
{
    bb_data_t *data = dr_global_alloc(sizeof(bb_data_t));
    char text[256];
    instr_t *instr;
    rc_classification_t cls;

    memset(data, 0, sizeof(*data));
    for (instr = instrlist_first_app(bb); instr != NULL;
         instr = instr_get_next_app(instr)) {
        data->instructions++;
        if (data->vaddr == 0)
            data->vaddr = (uint64_t)(uintptr_t)instr_get_app_pc(instr);
        if (instr_get_next_app(instr) == NULL) {
            data->end_vaddr = (uint64_t)(uintptr_t)instr_get_app_pc(instr) +
                instr_length(drcontext, instr);
            data->flow = flow_kind(instr);
        }
        disassemble_instr(drcontext, instr, text, sizeof(text));
        rc_classify(target, text, &cls);
        if (cls.kind == RC_KIND_STATIC) {
            data->cost.scalar_int += cls.cost.scalar_int;
            data->cost.scalar_float += cls.cost.scalar_float;
            data->cost.scalar_double += cls.cost.scalar_double;
            data->cost.vector_int += cls.cost.vector_int;
            data->cost.vector_float += cls.cost.vector_float;
            data->cost.vector_double += cls.cost.vector_double;
        }
        if (!memory_profile && target == RC_TARGET_X86) {
            int i;
            for (i = 0; i < instr_num_srcs(instr); i++) {
                opnd_t op = instr_get_src(instr, i);
                if (opnd_is_memory_reference(op) && instr_reads_memory(instr))
                    data->arch_bytes_load +=
                        opnd_size_in_bytes(opnd_get_size(op));
            }
            for (i = 0; i < instr_num_dsts(instr); i++) {
                opnd_t op = instr_get_dst(instr, i);
                if (opnd_is_memory_reference(op) && instr_writes_memory(instr))
                    data->arch_bytes_store +=
                        opnd_size_in_bytes(opnd_get_size(op));
            }
        }
    }

    data->handle = rc_register_block(session, data->vaddr, data->end_vaddr,
                                     data->flow, &data->cost, data->instructions,
                                     data->arch_bytes_load,
                                     data->arch_bytes_store);
    instr_t *last = instrlist_last_app(bb);
    if (last != NULL && (instr_is_cbr(last) || instr_is_ubr(last))) {
        opnd_t target_opnd = instr_get_target(last);
        if (opnd_is_pc(target_opnd)) {
            data->successors[data->successor_count++] =
                (uint64_t)(uintptr_t)opnd_get_pc(target_opnd);
        }
        if (instr_is_cbr(last) && data->successor_count < 2)
            data->successors[data->successor_count++] = data->end_vaddr;
    } else if (data->flow == RC_FLOW_NORMAL && last != NULL &&
               !instr_is_mbr(last)) {
        data->successors[data->successor_count++] = data->end_vaddr;
    }

    dr_mutex_lock(bb_list_lock);
    data->next = bb_list;
    bb_list = data;
    dr_mutex_unlock(bb_list_lock);
    *user_data = data;
    return DR_EMIT_DEFAULT;
}

/* Inline-writes one rc_record_t with no payload address (block-exec and
 * unclassified records; the address field is left uninitialized and ignored
 * by roofline-core). */
static void
insert_record(void *drcontext, instrlist_t *bb, instr_t *where, uint32_t desc)
{
    reg_id_t reg_ptr, reg_scratch;
    if (drreg_reserve_register(drcontext, bb, where, NULL, &reg_ptr) !=
            DRREG_SUCCESS ||
        drreg_reserve_register(drcontext, bb, where, NULL, &reg_scratch) !=
            DRREG_SUCCESS) {
        DR_ASSERT(false);
        return;
    }
    drx_buf_insert_load_buf_ptr(drcontext, record_buf, bb, where, reg_ptr);
    drx_buf_insert_buf_store(drcontext, record_buf, bb, where, reg_ptr,
                             reg_scratch, OPND_CREATE_INT32((int)desc), OPSZ_4,
                             offsetof(rc_record_t, desc));
    drx_buf_insert_update_buf_ptr(drcontext, record_buf, bb, where, reg_ptr,
                                  reg_scratch, sizeof(rc_record_t));
    drreg_unreserve_register(drcontext, bb, where, reg_scratch);
    drreg_unreserve_register(drcontext, bb, where, reg_ptr);
}

#ifdef X86
static void
insert_block_counter(void *drcontext, instrlist_t *bb, instr_t *where,
                     uint64_t *counter)
{
    /* drx_insert_counter_update always spills arithmetic flags when called
     * through drmgr. Most compute-loop bodies overwrite them before reading
     * them, so avoid that dominant cost when liveness proves it is safe. */
    bool preserve_aflags = !drx_aflags_are_dead(where);
    if (preserve_aflags &&
        drreg_reserve_aflags(drcontext, bb, where) != DRREG_SUCCESS) {
        DR_ASSERT(false);
        return;
    }
    instrlist_meta_preinsert(
        bb, where,
        LOCK(INSTR_CREATE_add(drcontext, OPND_CREATE_ABSMEM(counter, OPSZ_8),
                              OPND_CREATE_INT8(1))));
    if (preserve_aflags)
        drreg_unreserve_aflags(drcontext, bb, where);
}
#endif

static void
instrument_mem_refs(void *drcontext, instrlist_t *bb, instr_t *instr,
                    uint64_t block)
{
    int i;
    reg_id_t reg_addr, reg_ptr, reg_scratch;
    bool reserved = false;

    for (i = 0; i < instr_num_srcs(instr) + instr_num_dsts(instr); i++) {
        bool is_store = i >= instr_num_srcs(instr);
        opnd_t op = is_store ? instr_get_dst(instr, i - instr_num_srcs(instr))
                             : instr_get_src(instr, i);
        if (!opnd_is_memory_reference(op))
            continue;
        /* Skip address-generation pseudo-references (x86 lea). */
        if (!is_store && !instr_reads_memory(instr))
            continue;
        if (is_store && !instr_writes_memory(instr))
            continue;
        uint size = opnd_size_in_bytes(opnd_get_size(op));
        if (size == 0)
            continue;
        uint32_t handle = rc_register_mem(session, block, size, is_store ? 1 : 0);
        if (handle == UINT32_MAX)
            continue;
        if (!reserved) {
            if (drreg_reserve_register(drcontext, bb, instr, NULL, &reg_addr) !=
                    DRREG_SUCCESS ||
                drreg_reserve_register(drcontext, bb, instr, NULL, &reg_ptr) !=
                    DRREG_SUCCESS ||
                drreg_reserve_register(drcontext, bb, instr, NULL, &reg_scratch) !=
                    DRREG_SUCCESS) {
                DR_ASSERT(false);
                return;
            }
            reserved = true;
        }
        if (!drutil_insert_get_mem_addr(drcontext, bb, instr, op, reg_addr,
                                        reg_scratch))
            continue;
        drx_buf_insert_load_buf_ptr(drcontext, record_buf, bb, instr, reg_ptr);
        drx_buf_insert_buf_store(drcontext, record_buf, bb, instr, reg_ptr,
                                 reg_scratch,
                                 OPND_CREATE_INT32(
                                     (int)((handle << 2) | RC_RECORD_MEM)),
                                 OPSZ_4, offsetof(rc_record_t, desc));
        drx_buf_insert_buf_store(drcontext, record_buf, bb, instr, reg_ptr,
                                 reg_scratch, opnd_create_reg(reg_addr), OPSZ_8,
                                 offsetof(rc_record_t, address));
        drx_buf_insert_update_buf_ptr(drcontext, record_buf, bb, instr, reg_ptr,
                                      reg_scratch, sizeof(rc_record_t));
    }
    if (reserved) {
        drreg_unreserve_register(drcontext, bb, instr, reg_scratch);
        drreg_unreserve_register(drcontext, bb, instr, reg_ptr);
        drreg_unreserve_register(drcontext, bb, instr, reg_addr);
    }
}

static dr_emit_flags_t
event_bb_insertion(void *drcontext, void *tag, instrlist_t *bb, instr_t *instr,
                   bool for_trace, bool translating, void *user_data)
{
    bb_data_t *data = (bb_data_t *)user_data;
    char text[256];
    rc_classification_t cls;

    if (!instr_is_app(instr))
        return DR_EMIT_DEFAULT;

    if (instr == instrlist_first_app(bb) && data->handle != UINT32_MAX) {
#ifdef X86
        if (!memory_profile)
            insert_block_counter(drcontext, bb, instr, &data->executions);
        else
            insert_record(drcontext, bb, instr,
                          (data->handle << 2) | RC_RECORD_BLOCK_EXEC);
#else
        insert_record(drcontext, bb, instr,
                      (data->handle << 2) | RC_RECORD_BLOCK_EXEC);
#endif
    }

    disassemble_instr(drcontext, instr, text, sizeof(text));
    rc_classify(target, text, &cls);
    if (debug_classify) {
        dr_fprintf(STDERR,
                   "miniperf dr-roofline: classify kind=%u si=%llu sf=%llu sd=%llu vi=%llu vf=%llu vd=%llu '%s'\n",
                   cls.kind, cls.cost.scalar_int, cls.cost.scalar_float,
                   cls.cost.scalar_double, cls.cost.vector_int,
                   cls.cost.vector_float, cls.cost.vector_double, text);
    }
    if (cls.kind == RC_KIND_UNCLASSIFIED) {
        if (debug_unclassified)
            dr_fprintf(STDERR, "miniperf dr-roofline: unclassified '%s'\n", text);
        if (data->handle != UINT32_MAX) {
            insert_record(drcontext, bb, instr,
                          (data->handle << 2) | RC_RECORD_UNCLASSIFIED);
        }
    }
#ifdef RISCV64
    if (cls.kind == RC_KIND_RVV) {
        rvv_info_t *info = dr_global_alloc(sizeof(rvv_info_t));
        info->block = data->vaddr;
        info->is_float = cls.rvv_is_float;
        info->masked = cls.rvv_masked;
        info->factor = cls.rvv_factor;
        info->sew_scale = cls.rvv_sew_scale;
        dr_mutex_lock(bb_list_lock);
        info->next = rvv_list;
        rvv_list = info;
        dr_mutex_unlock(bb_list_lock);
        dr_insert_clean_call(drcontext, bb, instr, (void *)rvv_event, false, 1,
                             OPND_CREATE_INTPTR(info));
    }
#endif

    if ((memory_profile || target != RC_TARGET_X86) &&
        (instr_reads_memory(instr) || instr_writes_memory(instr)))
        instrument_mem_refs(drcontext, bb, instr, data->vaddr);

    return DR_EMIT_DEFAULT;
}

/* ---------------- lifecycle ---------------- */

static void
progress_thread(void *arg)
{
    rc_session_t *progress_session = (rc_session_t *)arg;
    for (;;) {
        uint64_t instructions = rc_instruction_count(progress_session);
#ifdef X86
        /* Roofline mode uses inline aggregate counters, while memory mode
         * emits block records and advances the session count as buffers are
         * consumed. Reading the aggregate counters in memory mode kept the
         * progress bar pinned at zero because those counters are not emitted. */
        if (!memory_profile) {
            uint64_t aggregate = 0;
            dr_mutex_lock(bb_list_lock);
            for (bb_data_t *data = bb_list; data != NULL; data = data->next) {
                uint64_t executions = (uint64_t)dr_atomic_load64(
                    (volatile int64 *)&data->executions);
                uint64_t block_instructions = executions > 0 &&
                        data->instructions > UINT64_MAX / executions
                    ? UINT64_MAX
                    : executions * data->instructions;
                aggregate = UINT64_MAX - aggregate < block_instructions
                    ? UINT64_MAX
                    : aggregate + block_instructions;
            }
            dr_mutex_unlock(bb_list_lock);
            instructions = aggregate;
        }
#endif
        dr_fprintf(progress_file, "%llu\n", instructions);
        dr_sleep(100);
    }
}

static void
event_thread_init(void *drcontext)
{
    uint index = dr_atomic_add32_return_sum((volatile int *)&thread_counter, 1) - 1;
    drmgr_set_tls_field(drcontext, tls_index, (void *)(uintptr_t)index);
}

static void
event_fork_init(void *drcontext)
{
    /* Artifacts describe the root process only, matching the QEMU plugin's
     * child_process_seen semantics. */
    in_child = true;
}

static void
event_exit(void)
{
    /* Thread exit events (including drx_buf's final flush of each thread's
     * partial buffer via flush_records) have already run by now. */
    if (record_buf != NULL) {
        drx_buf_free(record_buf);
        record_buf = NULL;
    }
    if (!in_child && session != NULL) {
        dr_mutex_lock(bb_list_lock);
        for (bb_data_t *data = bb_list; data != NULL; data = data->next) {
            if (data->executions == 0)
                continue;
            uint64_t executed_successors[2] = { 0, 0 };
            uint32_t executed_count = 0;
            for (uint32_t i = 0; i < data->successor_count; i++) {
                for (bb_data_t *candidate = bb_list; candidate != NULL;
                     candidate = candidate->next) {
                    if (candidate->executions != 0 &&
                        candidate->vaddr == data->successors[i]) {
                        executed_successors[executed_count++] =
                            data->successors[i];
                        break;
                    }
                }
            }
            rc_counted_block(session, data->handle, data->executions,
                             executed_successors[0], executed_successors[1],
                             executed_count);
        }
        dr_mutex_unlock(bb_list_lock);
        if (progress_file != INVALID_FILE)
            dr_fprintf(progress_file, "%llu\n", rc_instruction_count(session));
        rc_finalize(session);
    }
    session = NULL;
    if (progress_file != INVALID_FILE) {
        dr_close_file(progress_file);
        progress_file = INVALID_FILE;
    }

    dr_mutex_lock(bb_list_lock);
    while (bb_list != NULL) {
        bb_data_t *next = bb_list->next;
        dr_global_free(bb_list, sizeof(bb_data_t));
        bb_list = next;
    }
    while (rvv_list != NULL) {
        rvv_info_t *next = rvv_list->next;
        dr_global_free(rvv_list, sizeof(rvv_info_t));
        rvv_list = next;
    }
    dr_mutex_unlock(bb_list_lock);
    dr_mutex_destroy(bb_list_lock);

    drmgr_unregister_tls_field(tls_index);
    drx_exit();
    drreg_exit();
    drutil_exit();
    drmgr_exit();
}

static uint64_t
parse_u64_option(int argc, const char *argv[], const char *name,
                 uint64_t default_value)
{
    size_t len = strlen(name);
    int i;
    for (i = 1; i < argc; i++) {
        if (strncmp(argv[i], name, len) == 0 && argv[i][len] == '=')
            return strtoull(argv[i] + len + 1, NULL, 10);
    }
    return default_value;
}

static const char *
parse_str_option(int argc, const char *argv[], const char *name,
                 const char *default_value)
{
    size_t len = strlen(name);
    int i;
    for (i = 1; i < argc; i++) {
        if (strncmp(argv[i], name, len) == 0 && argv[i][len] == '=')
            return argv[i] + len + 1;
    }
    return default_value;
}

DR_EXPORT void
dr_client_main(client_id_t id, int argc, const char *argv[])
{
    dr_set_client_name("miniperf dr-roofline", "https://github.com/frontiers-labs");

#if defined(X86)
    target = RC_TARGET_X86;
#elif defined(RISCV64)
    target = RC_TARGET_RISCV;
#elif defined(AARCH64)
    target = RC_TARGET_AARCH64;
#else
#    error unsupported architecture
#endif

    const char *output = parse_str_option(argc, argv, "output", "dr-roofline.counts");
    uint64_t cache_line = parse_u64_option(argc, argv, "cache-line", DEFAULT_CACHE_LINE);
    uint64_t llc_size = parse_u64_option(argc, argv, "llc-size", DEFAULT_LLC_SIZE);
    uint64_t llc_assoc = parse_u64_option(argc, argv, "llc-assoc", DEFAULT_LLC_ASSOC);
    const char *profile = parse_str_option(argc, argv, "memory-profile", "on");
    const char *progress = parse_str_option(argc, argv, "progress", "");
    memory_profile = strcmp(profile, "off") != 0;
    debug_unclassified = getenv("MPERF_DR_DEBUG_UNCLASSIFIED") != NULL;
    debug_classify = getenv("MPERF_DR_DEBUG_CLASSIFY") != NULL;
    bb_list_lock = dr_mutex_create();

    session = rc_session_new(output, cache_line, llc_size, llc_assoc, memory_profile);
    if (session == NULL) {
        dr_fprintf(STDERR,
                   "miniperf dr-roofline: invalid cache-line/llc-size/llc-assoc\n");
        dr_abort();
    }
    if (progress[0] != '\0') {
        progress_file = dr_open_file(
            progress, DR_FILE_WRITE_OVERWRITE | DR_FILE_CLOSE_ON_FORK);
        if (progress_file != INVALID_FILE) {
            dr_fprintf(progress_file, "0\n");
            if (!dr_create_client_thread(progress_thread, session)) {
                dr_close_file(progress_file);
                progress_file = INVALID_FILE;
            }
        }
    }

    module_data_t *main_module = dr_get_main_module();
    if (main_module != NULL) {
        /* The artifact's image start must be the runtime address of the
         * executable text, matching the QEMU plugin's qemu_plugin_start_code():
         * mperf derives the module load bias as image_start minus the
         * executable ELF segment's link address. main_module->start is the
         * module base (the mapping of file offset 0), which for a PIE sits one
         * page below the text segment, so reporting it would skew every loop's
         * module offset and symbolize loops to unrelated symbols. */
        uint64_t text_start = (uint64_t)(uintptr_t)main_module->start;
#ifdef UNIX
        bool text_found = false;
        for (uint i = 0; i < main_module->num_segments; i++) {
            if ((main_module->segments[i].prot & DR_MEMPROT_EXEC) == 0)
                continue;
            uint64_t start = (uint64_t)(uintptr_t)main_module->segments[i].start;
            if (!text_found || start < text_start) {
                text_start = start;
                text_found = true;
            }
        }
#endif
        rc_session_set_image(session, text_start,
                             (uint64_t)(uintptr_t)main_module->end,
                             (uint64_t)(uintptr_t)main_module->entry_point);
        dr_free_module_data(main_module);
    }

    drreg_options_t ops = { sizeof(ops), 5 /*spill slots*/, false };
    if (!drmgr_init() || !drutil_init() || drreg_init(&ops) != DRREG_SUCCESS ||
        !drx_init())
        DR_ASSERT(false);
    record_buf = drx_buf_create_trace_buffer(RECORD_BUF_SIZE, flush_records);
    if (record_buf == NULL) {
        dr_fprintf(STDERR, "miniperf dr-roofline: failed to create trace buffer\n");
        dr_abort();
    }
    tls_index = drmgr_register_tls_field();
    DR_ASSERT(tls_index != -1);
    if (!drmgr_register_thread_init_event(event_thread_init) ||
        !drmgr_register_bb_app2app_event(event_bb_app2app, NULL) ||
        !drmgr_register_bb_instrumentation_event(event_bb_analysis,
                                                 event_bb_insertion, NULL))
        DR_ASSERT(false);
    dr_register_fork_init_event(event_fork_init);
    drmgr_register_exit_event(event_exit);
}
