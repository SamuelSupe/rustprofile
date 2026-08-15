use std::hint::black_box;

#[inline(never)]
fn burn(mut value: u64) -> u64 {
    for index in 0..50_000 {
        value = value
            .wrapping_mul(1_664_525)
            .wrapping_add(index)
            .rotate_left(7);
    }
    value
}

#[inline(never)]
fn layer_three(value: u64) -> u64 {
    burn(value)
}

#[inline(never)]
fn layer_two(value: u64) -> u64 {
    layer_three(value)
}

#[inline(never)]
fn layer_one(value: u64) -> u64 {
    layer_two(value)
}

fn main() {
    let mut value = 1_u64;
    loop {
        value = layer_one(value);
        black_box(value);
    }
}
