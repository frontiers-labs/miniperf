use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::measurement::{Measurement, ValueFormatter, WallTime};
use pmu_data::arith_parser::{try_parse_expr, BinOp, Expr};
use pmu_data::TmaScenario;

use crate::{CounterCheckpoint, EventTimer};

/// Wall-clock Criterion measurement with a shared top-down counter collector.
///
/// Pass a clone to `Criterion::with_measurement`, then call [`Self::report`]
/// after each benchmark. Clones share counters and never repeat the workload.
/// Counter collection covers all measured regions, including warmup and quick
/// calibration. Each coherent group uses only intervals in which it ran.
/// Counters belong to the calling thread; worker threads are not included.
#[derive(Clone)]
pub struct TMAMetrics(Rc<State>);

struct State {
    scenario: Option<TmaScenario>,
    groups: Vec<Group>,
    scopes: Cell<u64>,
    warned: Cell<bool>,
}

struct Group {
    name: String,
    events: Vec<String>,
    timer: Option<EventTimer>,
    totals: RefCell<Vec<f64>>,
    available: Cell<bool>,
}

impl TMAMetrics {
    /// Detects the host TMA configuration and attempts to open its counter groups.
    ///
    /// Missing configuration, unsupported events, or denied PMU access leave
    /// wall-clock benchmarking operational and counters unavailable.
    pub fn new() -> Self {
        let scenario = crate::tma_scenario();
        let mut groups = Vec::new();
        let mut error = None;
        if let Some(scenario) = &scenario {
            for group in &scenario.groups {
                let counters: Vec<_> = group
                    .events
                    .iter()
                    .map(|name| crate::tma_counter(name))
                    .collect();
                let timer = if group.cpus.is_some() {
                    Err(
                        "per-cluster TMA is unavailable for a migrating benchmark thread"
                            .to_owned(),
                    )
                } else {
                    EventTimer::new(&counters).map_err(|error| error.to_string())
                };
                let timer = timer.map_err(|reason| error = Some(reason)).ok();
                groups.push(Group {
                    name: group.name.clone(),
                    events: group.events.clone(),
                    timer,
                    totals: RefCell::new(vec![0.0; counters.len()]),
                    available: Cell::new(false),
                });
            }
        }
        let metrics = Self(Rc::new(State {
            scenario,
            groups,
            scopes: Cell::new(0),
            warned: Cell::new(false),
        }));
        if let Some(error) = error {
            metrics.warn(&error);
        }
        metrics
    }

    /// Prints named counter totals and TMA percentages, then clears the totals.
    ///
    /// Counts are multiplex-scaled totals, not counts per benchmark iteration.
    /// Unavailable values print as zero with an explicit marker. No output is
    /// produced if no measured region ran since the previous report, including
    /// benchmarks excluded by Criterion's filter.
    pub fn report(&self, name: &str) {
        if self.0.scopes.replace(0) == 0 {
            return;
        }
        let mut output = std::io::stdout().lock();
        let _ = writeln!(output, "TMA {name}:");
        let mut counts = Vec::new();
        for group in &self.0.groups {
            let totals = group.totals.replace(vec![0.0; group.events.len()]);
            let available = group.available.replace(false);
            for (event, count) in group.events.iter().zip(&totals) {
                let marker = if available { "" } else { " (unavailable)" };
                let _ = writeln!(output, "  {}/{event}: {count:.0}{marker}", group.name);
            }
            counts.push((totals, available));
        }
        let Some(scenario) = &self.0.scenario else {
            let _ = writeln!(output, "  unavailable");
            return;
        };
        let constants = scenario
            .constants
            .iter()
            .map(|c| (c.name.clone(), c.value as f64))
            .collect();
        let mut values = HashMap::new();
        for metric in &scenario.metrics {
            let value = self
                .0
                .groups
                .iter()
                .position(|g| Some(&g.name) == metric.group.as_ref())
                .and_then(|index| {
                    let (totals, available) = &counts[index];
                    if !available {
                        return None;
                    }
                    let mut inputs = values.clone();
                    inputs.extend(
                        self.0.groups[index]
                            .events
                            .iter()
                            .cloned()
                            .zip(totals.iter().copied()),
                    );
                    evaluate(&try_parse_expr(&metric.formula).ok()?, &inputs, &constants)
                        .filter(|value| (value * 100.0).is_finite())
                });
            if let Some(value) = value {
                values.insert(metric.name.clone(), value);
            }
            let marker = if value.is_some() {
                ""
            } else {
                " (unavailable)"
            };
            let _ = writeln!(
                output,
                "  {}: {:.2}%{marker}",
                metric.name,
                value.unwrap_or(0.0) * 100.0
            );
        }
    }

    fn warn(&self, error: &str) {
        if !self.0.warned.replace(true) {
            let _ = writeln!(
                std::io::stderr().lock(),
                "TMA counter interval unavailable: {error}; wall-clock benchmarking continues"
            );
        }
    }
}

impl Default for TMAMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Measurement for TMAMetrics {
    type Intermediate = (Instant, Vec<Option<CounterCheckpoint>>);
    type Value = Duration;

    fn start(&self) -> Self::Intermediate {
        let checkpoints = self
            .0
            .groups
            .iter()
            .map(|group| match group.timer.as_ref()?.checkpoint() {
                Ok(checkpoint) => Some(checkpoint),
                Err(error) => {
                    self.warn(&error.to_string());
                    None
                }
            })
            .collect();
        (Instant::now(), checkpoints)
    }

    fn end(&self, (started, checkpoints): Self::Intermediate) -> Self::Value {
        let elapsed = started.elapsed();
        self.0.scopes.set(self.0.scopes.get().saturating_add(1));
        for (group, checkpoint) in self.0.groups.iter().zip(checkpoints) {
            let (Some(timer), Some(checkpoint)) = (&group.timer, checkpoint) else {
                continue;
            };
            match timer.since(checkpoint) {
                Ok(counts) => {
                    for (total, count) in group.totals.borrow_mut().iter_mut().zip(counts.iter()) {
                        *total += count.value() as f64;
                    }
                    group.available.set(true);
                }
                Err(error) => self.warn(&error.to_string()),
            }
        }
        elapsed
    }

    fn add(&self, left: &Duration, right: &Duration) -> Duration {
        left.saturating_add(*right)
    }

    fn zero(&self) -> Duration {
        Duration::ZERO
    }

    fn to_f64(&self, value: &Duration) -> f64 {
        WallTime.to_f64(value)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        WallTime.formatter()
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
