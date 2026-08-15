#include <stdint.h>

static volatile uint64_t sink;

/*
 * The C controls deliberately clear the inherited frame-pointer register so
 * that `--unwind auto` must exercise the DWARF fallback. This is a synthetic
 * calibration fixture, not a claim about a compiler's default release mode.
 */
__attribute__((noinline)) static void synthetic_clear_inherited_frame_pointer(void) {
#if defined(__aarch64__)
    __asm__ volatile("mov x29, xzr" ::: "x29", "memory");
#elif defined(__x86_64__)
    __asm__ volatile("xor %%rbp, %%rbp" ::: "rbp", "memory");
#else
#error "synthetic frame-pointer fixture requires an AArch64 or x86_64 target"
#endif
}

__attribute__((noinline)) static uint64_t layer_three(uint64_t value) {
    return value ^ UINT64_C(0x9e3779b97f4a7c15);
}

__attribute__((noinline)) static uint64_t layer_two(uint64_t value) {
    return layer_three(value + UINT64_C(0x100000001b3));
}

__attribute__((noinline)) static uint64_t layer_one(uint64_t value) {
    return layer_two(value ^ UINT64_C(0xd6e8feb86659fd93));
}

__attribute__((noinline)) static uint64_t burn(uint64_t value) {
    for (uint64_t index = 0; index < 50000; ++index) {
        value = (value * UINT64_C(1664525) + index) ^ layer_one(index);
        value = (value << 7) | (value >> 57);
    }
    return value;
}

int main(void) {
    synthetic_clear_inherited_frame_pointer();
    uint64_t value = 1;
    for (;;) {
        value = burn(value);
        sink = value;
    }
}
