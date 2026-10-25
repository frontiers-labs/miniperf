use criterion::{black_box, Criterion};
use libprof::{Counter, CriterionCounter};

fn main() -> Result<(), libprof::Error> {
    let measurement = CriterionCounter::new(Counter::Instructions)?;
    let mut criterion = Criterion::default()
        .without_plots()
        .with_measurement(measurement);
    criterion.bench_function("sum instructions", |bencher| {
        bencher.iter(|| {
            let sum = (0_u64..1_000).fold(0_u64, |sum, value| sum.wrapping_add(value));
            black_box(sum)
        });
    });
    criterion.final_summary();
    Ok(())
}
