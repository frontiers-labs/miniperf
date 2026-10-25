use criterion::{BatchSize, Criterion};
use libprof::TMAMetrics;
use std::hint::black_box;

fn main() {
    let tma = TMAMetrics::new();
    let mut criterion = Criterion::default()
        .with_measurement(tma.clone())
        .configure_from_args();

    criterion.bench_function("tma/arithmetic", |bencher| {
        bencher.iter(|| {
            (0..10_000_u64).fold(black_box(1_u64), |acc, value| {
                acc.wrapping_mul(6364136223846793005).wrapping_add(value)
            })
        });
    });
    // Each report includes warmup and calibration scopes, then drains them.
    tma.report("tma/arithmetic");
    tma.report("empty");

    criterion.bench_function("tma/batched", |bencher| {
        bencher.iter_batched(
            || vec![1_u64; 1024],
            |mut values| {
                for value in &mut values {
                    *value = black_box(*value).wrapping_mul(3);
                }
                black_box(values)
            },
            BatchSize::SmallInput,
        );
    });
    tma.report("tma/batched");
    criterion.final_summary();
}
