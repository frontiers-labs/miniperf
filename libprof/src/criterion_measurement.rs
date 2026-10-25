//! Criterion measurement backed by an [`EventTimer`](crate::EventTimer).

use std::cell::RefCell;

use criterion::measurement::{Measurement as CriterionMeasurement, ValueFormatter};
use criterion::Throughput;

use crate::{Counter, CounterCheckpoint, Error, EventTimer};

/// Criterion measurement that reports one hardware-counter delta per batch.
///
/// Construct this type with one counter, then pass it to
/// `criterion::Criterion::with_measurement`. Runtime read errors cannot be
/// returned through Criterion's `Measurement` trait; they produce a zero value
/// and can be retrieved with [`CriterionCounter::take_error`].
pub struct CriterionCounter {
    timer: EventTimer,
    counter: Counter,
    formatter: CounterFormatter,
    error: RefCell<Option<Error>>,
}

impl CriterionCounter {
    /// Opens a per-thread Criterion measurement for `counter`.
    pub fn new(counter: Counter) -> Result<Self, Error> {
        let formatter = CounterFormatter::new(&counter);
        let timer = EventTimer::new(std::slice::from_ref(&counter))?;
        Ok(Self {
            timer,
            counter,
            formatter,
            error: RefCell::new(None),
        })
    }

    /// Takes the most recent counter read error, if Criterion encountered one.
    pub fn take_error(&self) -> Option<Error> {
        self.error.borrow_mut().take()
    }
}

impl CriterionMeasurement for CriterionCounter {
    type Intermediate = Option<CounterCheckpoint>;
    type Value = u64;

    fn start(&self) -> Self::Intermediate {
        match self.timer.checkpoint() {
            Ok(checkpoint) => Some(checkpoint),
            Err(error) => {
                *self.error.borrow_mut() = Some(error);
                None
            }
        }
    }

    fn end(&self, checkpoint: Self::Intermediate) -> Self::Value {
        let Some(checkpoint) = checkpoint else {
            return 0;
        };
        match self.timer.since(checkpoint) {
            Ok(measured) => measured[&self.counter],
            Err(error) => {
                *self.error.borrow_mut() = Some(error);
                0
            }
        }
    }

    fn add(&self, left: &Self::Value, right: &Self::Value) -> Self::Value {
        left.saturating_add(*right)
    }

    fn zero(&self) -> Self::Value {
        0
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        *value as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &self.formatter
    }
}

struct CounterFormatter {
    unit: &'static str,
    throughput_unit: &'static str,
}

impl CounterFormatter {
    fn new(counter: &Counter) -> Self {
        match counter {
            Counter::Cycles => Self {
                unit: "cycles",
                throughput_unit: "items/cycle",
            },
            Counter::Instructions => Self {
                unit: "instructions",
                throughput_unit: "items/instruction",
            },
            _ => Self {
                unit: "events",
                throughput_unit: "items/event",
            },
        }
    }
}

impl ValueFormatter for CounterFormatter {
    fn scale_values(&self, typical: f64, values: &mut [f64]) -> &'static str {
        let scale = if typical >= 1_000_000_000.0 {
            1e-9
        } else if typical >= 1_000_000.0 {
            1e-6
        } else if typical >= 1_000.0 {
            1e-3
        } else {
            1.0
        };
        for value in values {
            *value *= scale;
        }
        match (scale, self.unit) {
            (1e-9, "cycles") => "Gcycles",
            (1e-6, "cycles") => "Mcycles",
            (1e-3, "cycles") => "Kcycles",
            (1e-9, "instructions") => "Ginstructions",
            (1e-6, "instructions") => "Minstructions",
            (1e-3, "instructions") => "Kinstructions",
            (1e-9, _) => "Gevents",
            (1e-6, _) => "Mevents",
            (1e-3, _) => "Kevents",
            (_, unit) => unit,
        }
    }

    fn scale_throughputs(
        &self,
        _typical: f64,
        throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        let amount = match throughput {
            Throughput::Bytes(bytes) | Throughput::BytesDecimal(bytes) => *bytes as f64,
            Throughput::Elements(elements) => *elements as f64,
        };
        for value in values {
            *value = if *value == 0.0 { 0.0 } else { amount / *value };
        }
        self.throughput_unit
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        self.unit
    }
}
