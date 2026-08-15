#include <linux/bpf.h>
#include <linux/ptrace.h>
#include <linux/types.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define MAX_FRAMES 127
#define STACK_BYTES 16384
#define STACK_HALF (STACK_BYTES / 2)
#define LIVE_SAMPLE_CAPACITY 262144

enum event_kind {
    EVENT_STACK = 1,
    EVENT_ALLOC = 2,
    EVENT_FREE = 3,
};

enum operation {
    OP_ALLOC = 1,
    OP_REALLOC = 2,
    OP_POSIX_MEMALIGN = 3,
};

enum stat_index {
    STAT_RINGBUF_DROPS = 0,
    STAT_PENDING_OVERWRITES = 1,
    STAT_MAP_UPDATE_FAILURES = 2,
    STAT_STACK_FAILURES = 3,
    STAT_ALLOC_EVENTS = 4,
    STAT_SAMPLED_ALLOCS = 5,
    STAT_SAMPLED_FREES = 6,
    STAT_MAP_EVICTIONS = 7,
    STAT_COUNT = 8,
};

struct event_header {
    __u32 kind;
    __u32 unwind_mode;
    __u32 pid;
    __u32 tid;
    __u64 token;
    __u64 ptr;
    __u64 size;
    __u64 weight;
    __u64 ip;
    __u64 sp;
    __u64 fp;
    __u64 lr;
    __s32 stack_len;
    __u32 reserved;
};

struct fp_event {
    struct event_header header;
    __u64 ips[MAX_FRAMES];
};

struct dwarf_event {
    struct event_header header;
    __u8 stack[STACK_BYTES];
};

struct pending_allocation {
    __u64 token;
    __u64 size;
    __u64 weight;
    __u64 old_ptr;
    __u64 output_ptr;
    __u32 operation;
    __u32 sampled;
    __u32 old_tracked;
    __u32 reserved;
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);
    __type(value, struct pending_allocation);
} pending SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, LIVE_SAMPLE_CAPACITY);
    __type(key, __u64);
    __type(value, __u64);
} live_samples SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1 << 26);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, STAT_COUNT);
    __type(key, __u32);
    __type(value, __u64);
} stats SEC(".maps");

const volatile __u64 allocation_interval = 512 * 1024;
const volatile __u32 selected_unwind_mode = 1;
static __u64 next_token;
static __u64 live_count;

static __always_inline void increment_stat(__u32 index)
{
    __u64 *value = bpf_map_lookup_elem(&stats, &index);
    if (value)
        (*value)++;
}

static __always_inline void initialize_header(struct event_header *header, __u32 kind)
{
    __u64 id = bpf_get_current_pid_tgid();
    header->kind = kind;
    header->unwind_mode = selected_unwind_mode;
    header->pid = id >> 32;
    header->tid = (__u32)id;
    header->token = 0;
    header->ptr = 0;
    header->size = 0;
    header->weight = 0;
    header->ip = 0;
    header->sp = 0;
    header->fp = 0;
    header->lr = 0;
    header->stack_len = 0;
    header->reserved = 0;
}

static __always_inline __u64 sampling_weight(__u64 size)
{
    __u64 ratio;

    if (!size || size >= allocation_interval || !allocation_interval)
        return 1;
    ratio = allocation_interval / size;
    ratio |= ratio >> 1;
    ratio |= ratio >> 2;
    ratio |= ratio >> 4;
    ratio |= ratio >> 8;
    ratio |= ratio >> 16;
    ratio |= ratio >> 32;
    return ratio - (ratio >> 1);
}

static __always_inline int should_sample(__u64 weight)
{
    __u64 random;

    if (weight <= 1)
        return 1;
    random = bpf_get_prandom_u32();
    if (weight > (1ULL << 32))
        random = (random << 32) | bpf_get_prandom_u32();
    return (random & (weight - 1)) == 0;
}

static __always_inline void fill_registers(struct event_header *header, struct pt_regs *ctx)
{
#if defined(__TARGET_ARCH_arm64)
    header->ip = PT_REGS_IP(ctx);
    header->sp = PT_REGS_SP(ctx);
    header->fp = PT_REGS_FP(ctx);
    header->lr = PT_REGS_RET(ctx);
#else
    header->ip = PT_REGS_IP(ctx);
    header->sp = PT_REGS_SP(ctx);
    header->fp = PT_REGS_FP(ctx);
    header->lr = 0;
#endif
}

static __always_inline void emit_stack(struct pt_regs *ctx, __u64 token, __u64 size, __u64 weight)
{
    if (selected_unwind_mode == 1) {
        struct fp_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
        if (!event) {
            increment_stat(STAT_RINGBUF_DROPS);
            return;
        }
        initialize_header(&event->header, EVENT_STACK);
        event->header.token = token;
        event->header.size = size;
        event->header.weight = weight;
        fill_registers(&event->header, ctx);
        int length = bpf_get_stack(ctx, event->ips, sizeof(event->ips), BPF_F_USER_STACK);
        if (length < 0) {
            event->header.stack_len = 0;
            increment_stat(STAT_STACK_FAILURES);
        } else {
            event->header.stack_len = length;
        }
        bpf_ringbuf_submit(event, 0);
        return;
    }

    struct dwarf_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        increment_stat(STAT_RINGBUF_DROPS);
        return;
    }
    initialize_header(&event->header, EVENT_STACK);
    event->header.token = token;
    event->header.size = size;
    event->header.weight = weight;
    fill_registers(&event->header, ctx);

    int first = bpf_probe_read_user(event->stack, STACK_HALF, (void *)event->header.sp);
    if (first) {
        event->header.stack_len = 0;
        increment_stat(STAT_STACK_FAILURES);
    } else {
        event->header.stack_len = STACK_HALF;
        int second = bpf_probe_read_user(
            event->stack + STACK_HALF,
            STACK_HALF,
            (void *)(event->header.sp + STACK_HALF));
        if (!second)
            event->header.stack_len = STACK_BYTES;
    }
    bpf_ringbuf_submit(event, 0);
}

static __always_inline void emit_control(__u32 kind, __u64 token, __u64 ptr, __u64 size, __u64 weight)
{
    struct event_header *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        increment_stat(STAT_RINGBUF_DROPS);
        return;
    }
    initialize_header(event, kind);
    event->token = token;
    event->ptr = ptr;
    event->size = size;
    event->weight = weight;
    bpf_ringbuf_submit(event, 0);
}

static __always_inline int begin_allocation(
    struct pt_regs *ctx,
    __u64 size,
    __u64 old_ptr,
    __u64 output_ptr,
    __u32 operation)
{
    __u32 tid = (__u32)bpf_get_current_pid_tgid();
    struct pending_allocation value = {};
    struct pending_allocation *existing = bpf_map_lookup_elem(&pending, &tid);
    __u64 *old_token = 0;
    __u64 weight = sampling_weight(size);
    int sampled = size && should_sample(weight);

    increment_stat(STAT_ALLOC_EVENTS);
    if (existing)
        increment_stat(STAT_PENDING_OVERWRITES);
    if (old_ptr)
        old_token = bpf_map_lookup_elem(&live_samples, &old_ptr);
    if (!sampled && !old_token)
        return 0;

    value.size = size;
    value.weight = weight;
    value.old_ptr = old_ptr;
    value.output_ptr = output_ptr;
    value.operation = operation;
    value.sampled = sampled;
    value.old_tracked = old_token != 0;
    if (sampled) {
        value.token = __sync_fetch_and_add(&next_token, 1) + 1;
        emit_stack(ctx, value.token, size, weight);
    }
    if (bpf_map_update_elem(&pending, &tid, &value, BPF_ANY)) {
        increment_stat(STAT_MAP_UPDATE_FAILURES);
        return 0;
    }
    return 0;
}

static __always_inline int finish_allocation(struct pt_regs *ctx, int posix_memalign)
{
    __u32 tid = (__u32)bpf_get_current_pid_tgid();
    struct pending_allocation *value = bpf_map_lookup_elem(&pending, &tid);
    __u64 ptr = 0;

    if (!value)
        return 0;
    if (posix_memalign) {
        if (PT_REGS_RC(ctx) == 0 && value->output_ptr)
            bpf_probe_read_user(&ptr, sizeof(ptr), (void *)value->output_ptr);
    } else {
        ptr = PT_REGS_RC(ctx);
    }

    int realloc_succeeded = value->operation != OP_REALLOC || ptr || value->size == 0;
    if (value->operation == OP_REALLOC && value->old_tracked && realloc_succeeded) {
        __u64 *old_token = bpf_map_lookup_elem(&live_samples, &value->old_ptr);
        if (old_token) {
            emit_control(EVENT_FREE, *old_token, value->old_ptr, 0, 0);
            increment_stat(STAT_SAMPLED_FREES);
            if (!bpf_map_delete_elem(&live_samples, &value->old_ptr) && live_count)
                __sync_fetch_and_sub(&live_count, 1);
        }
    }

    if (value->sampled && ptr) {
        if (live_count >= LIVE_SAMPLE_CAPACITY)
            increment_stat(STAT_MAP_EVICTIONS);
        if (bpf_map_update_elem(&live_samples, &ptr, &value->token, BPF_ANY)) {
            increment_stat(STAT_MAP_UPDATE_FAILURES);
        } else {
            if (live_count < LIVE_SAMPLE_CAPACITY)
                __sync_fetch_and_add(&live_count, 1);
            emit_control(EVENT_ALLOC, value->token, ptr, value->size, value->weight);
            increment_stat(STAT_SAMPLED_ALLOCS);
        }
    }
    bpf_map_delete_elem(&pending, &tid);
    return 0;
}

static __always_inline int free_allocation(__u64 ptr)
{
    __u64 *token;
    if (!ptr)
        return 0;
    token = bpf_map_lookup_elem(&live_samples, &ptr);
    if (!token)
        return 0;
    emit_control(EVENT_FREE, *token, ptr, 0, 0);
    increment_stat(STAT_SAMPLED_FREES);
    if (!bpf_map_delete_elem(&live_samples, &ptr) && live_count)
        __sync_fetch_and_sub(&live_count, 1);
    return 0;
}

SEC("uprobe")
int rust_alloc_enter(struct pt_regs *ctx)
{
    return begin_allocation(ctx, PT_REGS_PARM1(ctx), 0, 0, OP_ALLOC);
}

SEC("uretprobe")
int rust_alloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int rust_alloc_zeroed_enter(struct pt_regs *ctx)
{
    return begin_allocation(ctx, PT_REGS_PARM1(ctx), 0, 0, OP_ALLOC);
}

SEC("uretprobe")
int rust_alloc_zeroed_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int rust_realloc_enter(struct pt_regs *ctx)
{
    return begin_allocation(
        ctx,
        PT_REGS_PARM4(ctx),
        PT_REGS_PARM1(ctx),
        0,
        OP_REALLOC);
}

SEC("uretprobe")
int rust_realloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int rust_dealloc_enter(struct pt_regs *ctx)
{
    return free_allocation(PT_REGS_PARM1(ctx));
}

SEC("uprobe")
int system_malloc_enter(struct pt_regs *ctx)
{
    return begin_allocation(ctx, PT_REGS_PARM1(ctx), 0, 0, OP_ALLOC);
}

SEC("uretprobe")
int system_malloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int system_calloc_enter(struct pt_regs *ctx)
{
    __u64 count = PT_REGS_PARM1(ctx);
    __u64 size = PT_REGS_PARM2(ctx);
    return begin_allocation(ctx, count * size, 0, 0, OP_ALLOC);
}

SEC("uretprobe")
int system_calloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int system_realloc_enter(struct pt_regs *ctx)
{
    return begin_allocation(
        ctx,
        PT_REGS_PARM2(ctx),
        PT_REGS_PARM1(ctx),
        0,
        OP_REALLOC);
}

SEC("uretprobe")
int system_realloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int system_free_enter(struct pt_regs *ctx)
{
    return free_allocation(PT_REGS_PARM1(ctx));
}

SEC("uprobe")
int system_aligned_alloc_enter(struct pt_regs *ctx)
{
    return begin_allocation(ctx, PT_REGS_PARM2(ctx), 0, 0, OP_ALLOC);
}

SEC("uretprobe")
int system_aligned_alloc_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 0);
}

SEC("uprobe")
int system_posix_memalign_enter(struct pt_regs *ctx)
{
    return begin_allocation(
        ctx,
        PT_REGS_PARM3(ctx),
        0,
        PT_REGS_PARM1(ctx),
        OP_POSIX_MEMALIGN);
}

SEC("uretprobe")
int system_posix_memalign_exit(struct pt_regs *ctx)
{
    return finish_allocation(ctx, 1);
}

char LICENSE[] SEC("license") = "GPL";
