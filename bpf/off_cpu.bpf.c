#include <linux/bpf.h>
#include <linux/types.h>
#include <bpf/bpf_helpers.h>

#define MAX_FRAMES 127

enum off_cpu_event_kind {
    EVENT_SWITCH_OUT = 1,
    EVENT_SWITCH_IN = 2,
};

enum stat_index {
    STAT_RINGBUF_DROPS = 0,
    STAT_STACK_FAILURES = 1,
    STAT_COUNT = 2,
};

struct trace_entry {
    __u16 type;
    __u8 flags;
    __u8 preempt_count;
    __s32 pid;
};

struct sched_switch_args {
    struct trace_entry entry;
    char prev_comm[16];
    __s32 prev_pid;
    __s32 prev_prio;
    long prev_state;
    char next_comm[16];
    __s32 next_pid;
    __s32 next_prio;
};

struct off_cpu_event_header {
    __u32 kind;
    __u32 pid;
    __u32 tid;
    __s32 stack_len;
    __u64 timestamp;
};

struct off_cpu_stack_event {
    struct off_cpu_event_header header;
    __u64 ips[MAX_FRAMES];
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);
    __type(value, __u32);
} tracked_tids SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1 << 24);
} off_cpu_events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, STAT_COUNT);
    __type(key, __u32);
    __type(value, __u64);
} off_cpu_stats SEC(".maps");

static __always_inline void increment_stat(__u32 index)
{
    __u64 *value = bpf_map_lookup_elem(&off_cpu_stats, &index);
    if (value)
        (*value)++;
}

static __always_inline void emit_switch_in(__u32 pid, __u32 tid)
{
    struct off_cpu_event_header *event = bpf_ringbuf_reserve(
        &off_cpu_events, sizeof(*event), 0);
    if (!event) {
        increment_stat(STAT_RINGBUF_DROPS);
        return;
    }
    event->kind = EVENT_SWITCH_IN;
    event->pid = pid;
    event->tid = tid;
    event->timestamp = bpf_ktime_get_ns();
    event->stack_len = 0;
    bpf_ringbuf_submit(event, 0);
}

static __always_inline void emit_switch_out(__u32 pid, __u32 tid, void *ctx)
{
    struct off_cpu_stack_event *event = bpf_ringbuf_reserve(
        &off_cpu_events, sizeof(*event), 0);
    if (!event) {
        increment_stat(STAT_RINGBUF_DROPS);
        return;
    }
    event->header.kind = EVENT_SWITCH_OUT;
    event->header.pid = pid;
    event->header.tid = tid;
    event->header.timestamp = bpf_ktime_get_ns();
    event->header.stack_len = 0;
    int length = bpf_get_stack(
        ctx, event->ips, sizeof(event->ips), BPF_F_USER_STACK);
    if (length < 0)
        increment_stat(STAT_STACK_FAILURES);
    else
        event->header.stack_len = length;
    bpf_ringbuf_submit(event, 0);
}

SEC("tracepoint/sched/sched_switch")
int sched_switch(struct sched_switch_args *ctx)
{
    __u32 prev_tid = ctx->prev_pid;
    __u32 next_tid = ctx->next_pid;
    __u32 *pid = bpf_map_lookup_elem(&tracked_tids, &prev_tid);
    if (pid)
        emit_switch_out(*pid, prev_tid, ctx);
    pid = bpf_map_lookup_elem(&tracked_tids, &next_tid);
    if (pid)
        emit_switch_in(*pid, next_tid);
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
