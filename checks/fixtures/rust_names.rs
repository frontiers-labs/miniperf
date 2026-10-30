use std::hint::black_box;

#[inline(never)]
fn generic<T>(value: u64) -> u64 {
    let mut result = value;
    for _ in 0..128 + std::mem::size_of::<T>() {
        result = result ^ (result << 7) ^ (result >> 3);
    }
    result
}

trait Run {
    fn run(&self) -> u64;
}
struct Worker<T>(T);
impl Run for Worker<u64> {
    #[inline(never)]
    fn run(&self) -> u64 {
        generic::<u64>(self.0)
    }
}

#[inline(always)]
fn inline_work(value: u64) -> u64 {
    black_box(value).wrapping_mul(3)
}

#[inline(never)]
fn closure_work(value: u64) -> u64 {
    let work = |x: u64| inline_work(black_box(x));
    black_box(work)(value)
}

mod left {
    #[inline(never)]
    pub fn same(value: u64) -> u64 { std::hint::black_box(value).wrapping_add(1) }
}
mod right {
    #[inline(never)]
    pub fn same(value: u64) -> u64 { std::hint::black_box(value).wrapping_add(2) }
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn plain_c_name(value: u64) -> u64 {
    black_box(value).wrapping_add(3)
}

fn main() {
    let count = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let mut result = 0u64;
    for value in 0..count {
        result = result.wrapping_add(generic::<u64>(value));
        result = result.wrapping_add(generic::<u32>(value));
        result = result.wrapping_add(generic::<Vec<Option<Result<[u64; 4], String>>>>(value));
        result = result.wrapping_add(Worker(value).run());
        result = result.wrapping_add(closure_work(value));
        result = result.wrapping_add(left::same(value));
        result = result.wrapping_add(right::same(value));
        result = result.wrapping_add(plain_c_name(value));
    }
    println!("{}", black_box(result));
}
