use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, SamplingMode};
use libprof::TMAMetrics;

fn arithmetic(criterion: &mut Criterion<TMAMetrics>) {
    let mut group = criterion.benchmark_group("tma");
    group.sampling_mode(SamplingMode::Flat);
    group.bench_function("arithmetic", |bencher| {
        bencher.iter(|| {
            let mut value = black_box(1_u64);
            for index in 0..4096_u64 {
                value = value.wrapping_mul(6364136223846793005).wrapping_add(index);
            }
            black_box(value)
        });
    });
    group.bench_function("batched", |bencher| {
        bencher.iter_batched(
            || black_box([1_u64; 64]),
            |mut values| {
                for value in &mut values {
                    *value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                black_box(values)
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .with_measurement(TMAMetrics::new())
        .without_plots()
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(400))
        .sample_size(10)
        .nresamples(1000);
    targets = arithmetic
}
criterion_main!(benches);
