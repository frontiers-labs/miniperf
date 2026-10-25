use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::{CounterCheckpoint, EventTimer};
use criterion::measurement::{Measurement, ValueFormatter, WallTime};
use pmu_data::arith_parser::{try_parse_expr, BinOp, Expr};
use pmu_data::TmaScenario;
use serde::Serialize;

/// Criterion wall-clock measurement with optional CPU top-down counters.
///
/// Pass this to `Criterion::with_measurement`. Hardware counters cover the
/// calling thread's measured regions, including Criterion's batched iterations.
/// Use the patched Criterion package from the same miniperf revision as libprof.
/// It supplies the batch and sample hooks required by this measurement.
/// Counter failures preserve elapsed-time measurements. Each benchmark writes
/// `new/tma.json` beside Criterion's results, with counts per iteration and TMA
/// percentages. Each counter records the iterations it observed; intervals
/// where its group did not run are excluded. Unavailable values are zero and
/// have `available: false`.
pub struct TMAMetrics {
    scenario: Option<TmaScenario>,
    groups: Vec<CounterGroup>,
    expressions: Vec<Option<Expr>>,
    notified: Cell<bool>,
}

struct CounterGroup {
    name: String,
    events: Vec<String>,
    timer: RefCell<Option<EventTimer>>,
    error: RefCell<Option<String>>,
}

/// Elapsed time and coherent counter totals for a Criterion sample.
pub struct TMASample {
    elapsed: Duration,
    groups: Vec<Vec<Count>>,
}

#[derive(Clone, Default)]
struct Count {
    raw: u64,
    scaled: u64,
    max_scaling: f64,
    iterations: u64,
}

#[derive(Serialize)]
struct Metric {
    value: f64,
    available: bool,
}

#[derive(Serialize)]
struct Counter {
    #[serde(flatten)]
    metric: Metric,
    raw: u64,
    max_scaling: f64,
    iterations: u64,
}

#[derive(Serialize)]
struct Report<'a> {
    benchmark: &'a str,
    samples: usize,
    iterations: u64,
    counters: BTreeMap<String, Counter>,
    metrics: BTreeMap<String, Metric>,
    errors: BTreeMap<String, String>,
}

impl TMAMetrics {
    /// Detects the host's TMA scenario and attempts to open its counter groups.
    ///
    /// Missing PMU support, configuration, or permissions disables counters
    /// without preventing benchmarks from running.
    pub fn new() -> Self {
        let scenario = crate::tma_scenario();
        let mut groups = Vec::new();
        let mut expressions = Vec::new();
        if let Some(scenario) = &scenario {
            for group in &scenario.groups {
                let counters: Vec<_> = group
                    .events
                    .iter()
                    .map(|event| crate::tma_counter(event))
                    .collect();
                let timer = if group.cpus.is_some() {
                    Err("per-cluster TMA is unavailable for a migrating benchmark thread".into())
                } else {
                    EventTimer::new(&counters).map_err(|error| error.to_string())
                };
                let (timer, error) = match timer {
                    Ok(timer) => (Some(timer), None),
                    Err(error) => (None, Some(error)),
                };
                groups.push(CounterGroup {
                    name: group.name.clone(),
                    events: group.events.clone(),
                    timer: RefCell::new(timer),
                    error: RefCell::new(error),
                });
            }
            expressions.extend(
                scenario
                    .metrics
                    .iter()
                    .map(|metric| try_parse_expr(&metric.formula).ok()),
            );
        }
        let measurement = Self {
            scenario,
            groups,
            expressions,
            notified: Cell::new(false),
        };
        if measurement.groups.is_empty() {
            measurement.notify("no TMA counter configuration for this CPU");
        } else if let Some(error) = measurement
            .groups
            .iter()
            .find_map(|group| group.error.borrow().clone())
        {
            measurement.notify(&error);
        }
        measurement
    }

    fn notify(&self, error: &str) {
        if !self.notified.replace(true) {
            eprintln!(
                "TMA counter interval unavailable: {error}. Wall-clock benchmarking continues."
            );
        }
    }

    fn disable(&self, group: &CounterGroup, error: crate::Error) {
        self.notify(&error.to_string());
        *group.error.borrow_mut() = Some(error.to_string());
        *group.timer.borrow_mut() = None;
    }

    fn report<'a>(
        &self,
        benchmark: &'a str,
        iterations: &[u64],
        samples: &[TMASample],
    ) -> Report<'a> {
        let total = samples
            .iter()
            .fold(self.zero(), |total, sample| self.add(&total, sample));
        let iterations = iterations
            .iter()
            .fold(0_u64, |sum, n| sum.saturating_add(*n));
        let mut counters = BTreeMap::new();
        let mut errors = BTreeMap::new();
        for (group, counts) in self.groups.iter().zip(&total.groups) {
            if let Some(error) = group.error.borrow().as_ref() {
                errors.insert(group.name.clone(), error.clone());
            }
            for (index, name) in group.events.iter().enumerate() {
                let count = &counts[index];
                counters.insert(
                    format!("{}/{name}", group.name),
                    Counter {
                        metric: Metric {
                            value: count.scaled as f64 / count.iterations.max(1) as f64,
                            available: count.iterations > 0,
                        },
                        raw: count.raw,
                        max_scaling: count.max_scaling,
                        iterations: count.iterations,
                    },
                );
            }
        }
        Report {
            benchmark,
            samples: samples.len(),
            iterations,
            counters,
            metrics: self.metrics(&total),
            errors,
        }
    }

    fn metrics(&self, total: &TMASample) -> BTreeMap<String, Metric> {
        let Some(scenario) = &self.scenario else {
            return ["retiring", "bad_speculation", "fe_bound", "be_bound"]
                .map(|name| {
                    (
                        name.into(),
                        Metric {
                            value: 0.0,
                            available: false,
                        },
                    )
                })
                .into();
        };
        let constants: HashMap<_, _> = scenario
            .constants
            .iter()
            .map(|constant| (constant.name.clone(), constant.value as f64))
            .collect();
        let mut values = HashMap::new();
        let mut metrics = BTreeMap::new();
        for (metric, expression) in scenario.metrics.iter().zip(&self.expressions) {
            let group = self
                .groups
                .iter()
                .position(|group| Some(&group.name) == metric.group.as_ref());
            let result = group.and_then(|index| {
                let counts = &total.groups[index];
                if !counts.iter().any(|count| count.iterations > 0) {
                    return None;
                }
                let mut inputs = values.clone();
                inputs.extend(
                    self.groups[index]
                        .events
                        .iter()
                        .zip(counts)
                        .map(|(name, count)| (name.clone(), count.scaled as f64)),
                );
                evaluate(expression.as_ref()?, &inputs, &constants)
            });
            if let Some(value) = result {
                values.insert(metric.name.clone(), value);
            }
            metrics.insert(
                metric.name.clone(),
                Metric {
                    value: result.map_or(0.0, |value| value * 100.0),
                    available: result.is_some(),
                },
            );
        }
        metrics
    }
}

impl Default for TMAMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Measurement for TMAMetrics {
    type Intermediate = (Instant, u64, Vec<Option<CounterCheckpoint>>);
    type Value = TMASample;

    fn start(&self) -> Self::Intermediate {
        self.start_batch(1)
    }

    fn start_batch(&self, iterations: u64) -> Self::Intermediate {
        let checkpoints = self
            .groups
            .iter()
            .map(|group| {
                let result = group
                    .timer
                    .borrow()
                    .as_ref()
                    .map(EventTimer::checkpoint)
                    .transpose();
                match result {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => {
                        self.disable(group, error);
                        None
                    }
                }
            })
            .collect();
        (Instant::now(), iterations, checkpoints)
    }

    fn end(&self, (started, iterations, checkpoints): Self::Intermediate) -> Self::Value {
        let elapsed = started.elapsed();
        let groups = self
            .groups
            .iter()
            .zip(checkpoints)
            .map(|(group, checkpoint)| {
                let measured = checkpoint.and_then(|checkpoint| {
                    let result = group.timer.borrow().as_ref()?.since(checkpoint);
                    match result {
                        Ok(measured) => {
                            *group.error.borrow_mut() = None;
                            Some(measured)
                        }
                        Err(error) => {
                            self.notify(&error.to_string());
                            *group.error.borrow_mut() = Some(error.to_string());
                            None
                        }
                    }
                });
                measured.map_or_else(
                    || vec![Count::default(); group.events.len()],
                    |measured| {
                        measured
                            .iter()
                            .map(|count| Count {
                                raw: count.raw(),
                                scaled: count.value(),
                                max_scaling: count.scaling(),
                                iterations,
                            })
                            .collect()
                    },
                )
            })
            .collect();
        TMASample { elapsed, groups }
    }

    fn add(&self, left: &Self::Value, right: &Self::Value) -> Self::Value {
        let groups = left
            .groups
            .iter()
            .zip(&right.groups)
            .map(|(left, right)| {
                left.iter()
                    .zip(right)
                    .map(|(left, right)| Count {
                        raw: left.raw.saturating_add(right.raw),
                        scaled: left.scaled.saturating_add(right.scaled),
                        max_scaling: left.max_scaling.max(right.max_scaling),
                        iterations: left.iterations.saturating_add(right.iterations),
                    })
                    .collect()
            })
            .collect();
        TMASample {
            elapsed: left.elapsed + right.elapsed,
            groups,
        }
    }

    fn zero(&self) -> Self::Value {
        TMASample {
            elapsed: Duration::ZERO,
            groups: self
                .groups
                .iter()
                .map(|group| vec![Count::default(); group.events.len()])
                .collect(),
        }
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        WallTime.to_f64(&value.elapsed)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        WallTime.formatter()
    }

    fn samples_complete(
        &self,
        benchmark: &str,
        directory: &Path,
        iterations: &[u64],
        samples: &[Self::Value],
    ) {
        let report = self.report(benchmark, iterations, samples);
        let summary = report
            .metrics
            .iter()
            .filter(|(name, _)| !name.contains('.'))
            .map(|(name, metric)| {
                if metric.available {
                    format!("{name} {:.2}%", metric.value)
                } else {
                    format!("{name} unavailable")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("{benchmark} TMA: {summary}");
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            fs::create_dir_all(directory)?;
            fs::write(
                directory.join("tma.json"),
                serde_json::to_vec_pretty(&report)?,
            )?;
            Ok(())
        };
        if let Err(error) = write() {
            eprintln!("Could not save TMA metrics for {benchmark}: {error}");
        }
    }
}

fn evaluate(
    expression: &Expr,
    values: &HashMap<String, f64>,
    constants: &HashMap<String, f64>,
) -> Option<f64> {
    let eval = |expression| evaluate(expression, values, constants);
    let value = match expression {
        Expr::Variable(name) => *values.get(name)?,
        Expr::Constant(name) => *constants.get(name)?,
        Expr::Num(value) => *value,
        Expr::Binary { op, lhs, rhs } => {
            let (left, right) = (eval(lhs)?, eval(rhs)?);
            match op {
                BinOp::Add => left + right,
                BinOp::Sub => left - right,
                BinOp::Mul => left * right,
                BinOp::Div => left / right,
                BinOp::Eq => u8::from(left == right) as f64,
                BinOp::Lt => u8::from(left < right) as f64,
                BinOp::Le => u8::from(left <= right) as f64,
                BinOp::Gt => u8::from(left > right) as f64,
                BinOp::Ge => u8::from(left >= right) as f64,
            }
        }
        Expr::Call { name, args } => match (name.to_ascii_lowercase().as_str(), args.as_slice()) {
            ("min", [left, right]) => eval(left)?.min(eval(right)?),
            ("max", [left, right]) => eval(left)?.max(eval(right)?),
            ("abs", [value]) => eval(value)?.abs(),
            ("if", [condition, yes, no]) => eval(if eval(condition)? != 0.0 { yes } else { no })?,
            _ => return None,
        },
    };
    value.is_finite().then_some(value)
}
