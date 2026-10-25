use criterion::measurement::{Measurement, ValueFormatter, WallTime};
use criterion::{BatchSize, Criterion};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

// Deliberately not Clone: retaining measurements must not add a Value bound.
struct Sample {
    iterations: u64,
    serial: u64,
}

struct ObservedMeasurement {
    serial: Rc<Cell<u64>>,
    reports: Rc<RefCell<Vec<Vec<(u64, u64)>>>>,
    routine_calls: Rc<Cell<u64>>,
    partial_batch: Rc<Cell<bool>>,
    output_directory: std::path::PathBuf,
}

impl Measurement for ObservedMeasurement {
    type Intermediate = (u64, u64);
    type Value = Sample;

    fn start(&self) -> Self::Intermediate {
        unreachable!("Criterion must supply the measured region's iteration count")
    }

    fn start_batch(&self, iterations: u64) -> Self::Intermediate {
        (iterations, self.routine_calls.get())
    }

    fn end(&self, (iterations, before): Self::Intermediate) -> Sample {
        assert_eq!(iterations, self.routine_calls.get() - before);
        self.partial_batch
            .set(self.partial_batch.get() || iterations < 3);
        self.serial.set(self.serial.get() + 1);
        Sample {
            iterations,
            serial: self.serial.get(),
        }
    }

    fn add(&self, a: &Sample, b: &Sample) -> Sample {
        Sample {
            iterations: a.iterations + b.iterations,
            serial: b.serial,
        }
    }

    fn zero(&self) -> Sample {
        Sample {
            iterations: 0,
            serial: 0,
        }
    }

    fn to_f64(&self, value: &Sample) -> f64 {
        value.iterations as f64 * 1024.0 + value.serial as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        WallTime.formatter()
    }

    fn samples_complete(
        &self,
        benchmark: &str,
        output_directory: &Path,
        iterations: &[u64],
        values: &[Sample],
    ) {
        assert_eq!(benchmark, "samples/retained?");
        assert_eq!(output_directory, self.output_directory);
        assert_eq!(iterations.len(), values.len());
        assert_eq!(values.last().unwrap().serial, self.serial.get());
        for (iterations, value) in iterations.iter().zip(values) {
            assert_eq!(*iterations, value.iterations);
        }
        self.reports.borrow_mut().push(
            values
                .iter()
                .map(|value| (value.iterations, value.serial))
                .collect(),
        );
    }
}

fn run_case(case: &str) {
    let directory = tempfile::tempdir().unwrap();
    let output_directory = directory.path().join("samples/retained_/new");
    let serial = Rc::new(Cell::new(0));
    let reports = Rc::new(RefCell::new(Vec::new()));
    let routine_calls = Rc::new(Cell::new(0));
    let partial_batch = Rc::new(Cell::new(false));
    let measurement = ObservedMeasurement {
        serial: Rc::clone(&serial),
        reports: Rc::clone(&reports),
        routine_calls: Rc::clone(&routine_calls),
        partial_batch: Rc::clone(&partial_batch),
        output_directory: output_directory.clone(),
    };
    let mut criterion = Criterion::default()
        .with_measurement(measurement)
        .output_directory(directory.path())
        .without_plots()
        .sample_size(10)
        .warm_up_time(Duration::from_nanos(1))
        .measurement_time(if case == "quick-long" {
            Duration::from_nanos(1)
        } else {
            Duration::from_millis(20)
        })
        .nresamples(100)
        .configure_from_args();
    criterion
        .benchmark_group("samples")
        .bench_function("retained?", |bencher| {
            let routine = || routine_calls.set(routine_calls.get() + 1);
            match case {
                "normal-iter" => bencher.iter(routine),
                "normal-batched" => {
                    bencher.iter_batched(|| (), |()| routine(), BatchSize::NumIterations(3));
                }
                "normal-batched-ref" => {
                    bencher.iter_batched_ref(
                        || (),
                        |_: &mut ()| routine(),
                        BatchSize::NumIterations(3),
                    );
                }
                "normal-single" => {
                    bencher.iter_batched(|| (), |()| routine(), BatchSize::PerIteration);
                }
                "normal-single-ref" => {
                    bencher.iter_batched_ref(
                        || (),
                        |_: &mut ()| routine(),
                        BatchSize::PerIteration,
                    );
                }
                _ => bencher.iter_custom(|iterations| {
                    std::thread::sleep(Duration::from_millis(2));
                    serial.set(serial.get() + 1);
                    Sample {
                        iterations,
                        serial: serial.get(),
                    }
                }),
            }
        });

    let reports = reports.borrow();
    if matches!(case, "test" | "profile") {
        assert!(reports.is_empty());
        assert!(!output_directory.exists());
        return;
    }
    assert_eq!(reports.len(), 1);
    let retained = &reports[0];
    assert_eq!(
        retained.len(),
        if case.starts_with("normal") {
            10
        } else if case == "quick" {
            2
        } else {
            1
        }
    );
    assert_eq!(retained.last().unwrap().1, serial.get());
    if case.starts_with("normal-") {
        assert!(retained.windows(2).all(|pair| pair[1].1 > pair[0].1));
        if matches!(case, "normal-batched" | "normal-batched-ref") {
            assert!(
                partial_batch.get(),
                "the final batch must use its actual size"
            );
        }
    } else {
        assert!(retained.windows(2).all(|pair| pair[1].1 == pair[0].1 + 1));
    }
    if case == "quick" {
        assert!(retained[0].1 > 1, "quick-mode trials must be discarded");
    }
    let scalar: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(output_directory.join("sample.json")).unwrap())
            .unwrap();
    let times = scalar["times"].as_array().unwrap();
    assert_eq!(
        times.len(),
        if case == "quick-long" {
            2
        } else {
            retained.len()
        }
    );
    for (index, &(iterations, serial)) in retained.iter().enumerate() {
        assert_eq!(scalar["iters"][index].as_f64().unwrap(), iterations as f64);
        assert_eq!(
            times[index].as_f64().unwrap(),
            iterations as f64 * 1024.0 + serial as f64
        );
    }
    if case == "quick-long" {
        assert_eq!(
            times[1].as_f64().unwrap(),
            times[0].as_f64().unwrap().next_up()
        );
    }
}

fn main() {
    if let Ok(case) = std::env::var("CRITERION_SAMPLE_HOOK_CASE") {
        run_case(&case);
        return;
    }
    for (case, args) in [
        ("normal", vec!["--bench"]),
        ("normal-iter", vec!["--bench"]),
        ("normal-batched", vec!["--bench"]),
        ("normal-batched-ref", vec!["--bench"]),
        ("normal-single", vec!["--bench"]),
        ("normal-single-ref", vec!["--bench"]),
        ("quick", vec!["--bench", "--quick"]),
        ("quick-long", vec!["--bench", "--quick"]),
        ("test", vec!["--bench", "--test"]),
        ("profile", vec!["--bench", "--profile-time", "1"]),
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .env("CRITERION_SAMPLE_HOOK_CASE", case)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
