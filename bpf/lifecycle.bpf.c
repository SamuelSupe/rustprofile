#include <linux/bpf.h>
#include <linux/types.h>
#include <bpf/bpf_helpers.h>

enum lifecycle_kind {
    LIFECYCLE_FORK = 1,
    LIFECYCLE_EXIT = 2,
    LIFECYCLE_EXEC = 3,
};

struct lifecycle_event {
    __u32 kind;
    __u32 tid;
};

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1 << 18);
} lifecycle_events SEC(".maps");

const volatile __u32 target_tgid;

static __always_inline int emit_if_target(__u32 kind)
{
    __u64 id = bpf_get_current_pid_tgid();
    if ((__u32)(id >> 32) != target_tgid)
        return 0;
    struct lifecycle_event *event = bpf_ringbuf_reserve(
        &lifecycle_events, sizeof(*event), 0);
    if (!event)
        return 0;
    event->kind = kind;
    event->tid = (__u32)id;
    bpf_ringbuf_submit(event, 0);
    return 0;
}

SEC("tracepoint/sched/sched_process_fork")
int process_fork(void *ctx)
{
    return emit_if_target(LIFECYCLE_FORK);
}

SEC("tracepoint/sched/sched_process_exit")
int process_exit(void *ctx)
{
    return emit_if_target(LIFECYCLE_EXIT);
}

SEC("tracepoint/sched/sched_process_exec")
int process_exec(void *ctx)
{
    return emit_if_target(LIFECYCLE_EXEC);
}

char LICENSE[] SEC("license") = "GPL";
