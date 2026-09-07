//! Quick, in-memory sampling of a closure.

#[cfg(feature = "symbolize")]
use std::collections::HashMap;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use crate::{Counter, Error, Record, Sample, SamplingDriverBuilder, UnwindMode};

/// A quick in-memory sampler for focused work.
///
/// Unlike the profiler command, this type creates no files and needs no async
/// runtime. The perf reader uses one plain background thread and is joined
/// before [`QuickSampler::record`] returns.
#[derive(Clone, Debug)]
pub struct QuickSampler {
    counters: Vec<Counter>,
    max_samples: Option<usize>,
    unwind_mode: UnwindMode,
    stack_dump_size: u32,
}

impl QuickSampler {
    /// Creates an unbounded sampler for the requested counters.
    pub fn new(counters: &[Counter]) -> Result<Self, Error> {
        if counters.is_empty() {
            return Err(Error::InvalidConfiguration(
                "QuickSampler requires at least one counter".to_owned(),
            ));
        }
        Ok(Self {
            counters: counters.to_vec(),
            max_samples: None,
            unwind_mode: UnwindMode::Dwarf,
            stack_dump_size: 8 * 1024,
        })
    }

    /// Creates a sampler retaining at most `max_samples` newest samples.
    pub fn bounded(counters: &[Counter], max_samples: usize) -> Result<Self, Error> {
        if max_samples == 0 {
            return Err(Error::InvalidConfiguration(
                "QuickSampler bounded capacity must be greater than zero".to_owned(),
            ));
        }
        let mut sampler = Self::new(counters)?;
        sampler.max_samples = Some(max_samples);
        Ok(sampler)
    }

    /// Selects frame-pointer or DWARF user-stack capture.
    pub fn unwind_mode(mut self, mode: UnwindMode) -> Self {
        self.unwind_mode = mode;
        self
    }

    /// Sets the maximum stack bytes captured for each DWARF sample.
    pub fn stack_dump_size(mut self, bytes: u32) -> Self {
        self.stack_dump_size = bytes;
        self
    }

    /// Samples `work` at `frequency_hz` and returns samples in memory.
    ///
    /// For a bounded sampler this returns only retained samples. Use
    /// [`QuickSampler::record_batch`] when the overwritten-sample count matters.
    pub fn record<F, R>(&self, frequency_hz: u64, work: F) -> Result<Vec<Sample>, Error>
    where
        F: FnOnce() -> R,
    {
        self.record_batch(frequency_hz, work)
            .map(SampleBatch::into_samples)
    }

    /// Samples `work`, returning retained samples and bounded-buffer drop count.
    pub fn record_batch<F, R>(&self, frequency_hz: u64, work: F) -> Result<SampleBatch, Error>
    where
        F: FnOnce() -> R,
    {
        if frequency_hz == 0 {
            return Err(Error::InvalidConfiguration(
                "sampling frequency must be greater than zero".to_owned(),
            ));
        }

        let collector = Arc::new(Mutex::new(Collector::new(self.max_samples)));
        let callback_collector = Arc::clone(&collector);
        let mut driver = SamplingDriverBuilder::new()
            .counters(&self.counters)
            .sample_freq(frequency_hz)
            .unwind_mode(self.unwind_mode)
            .stack_dump_size(self.stack_dump_size)
            .build()?;
        driver.start(Arc::new(move |record| {
            if let Record::Sample(sample) = record {
                callback_collector
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(sample);
            }
        }))?;

        let workload = catch_unwind(AssertUnwindSafe(work));
        let stop = driver.stop();
        if workload.is_err() {
            return Err(Error::WorkloadPanicked);
        }
        stop?;

        let mut collector = collector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(collector.take())
    }
}

/// Samples retained from one quick run.
#[derive(Debug, Default)]
pub struct SampleBatch {
    samples: Vec<Sample>,
    dropped_samples: u64,
}

impl SampleBatch {
    /// Returns retained samples in arrival order.
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// Consumes the batch and returns its samples.
    pub fn into_samples(self) -> Vec<Sample> {
        self.samples
    }

    /// Returns the number of samples overwritten by a bounded buffer.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Returns the number of retained samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Returns whether no samples were retained.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Collapses retained call stacks into the standard folded-stack format.
    ///
    /// With the `symbolize` feature this uses the shared miniperf resolver, so
    /// separate debug files, build-id cache entries, perf JIT maps, and DWARF
    /// inline frames follow the same rules as `mperf` postprocessing.
    #[cfg(feature = "symbolize")]
    pub fn to_folded(&self) -> String {
        let resolver = symbolize::Resolver::for_current_process().ok();
        let mut folded = HashMap::<String, u64>::new();
        for sample in &self.samples {
            let ips = if sample.callstack.is_empty() {
                std::slice::from_ref(&sample.ip)
            } else {
                sample.callstack.as_slice()
            };
            let mut names = Vec::new();
            for ip in ips {
                let frames = resolver
                    .as_ref()
                    .map(|resolver| resolver.resolve(sample.pid, *ip))
                    .unwrap_or_default();
                if frames.is_empty() {
                    names.push(
                        symbolize::current_process_symbol(*ip)
                            .unwrap_or_else(|| format!("0x{ip:x}")),
                    );
                } else {
                    names.extend(frames.into_iter().map(|frame| frame.function));
                }
            }
            names.reverse();
            let stack = names
                .into_iter()
                .map(|name| name.replace(';', ":"))
                .collect::<Vec<_>>()
                .join(";");
            *folded.entry(stack).or_default() += 1;
        }
        let mut lines = folded.into_iter().collect::<Vec<_>>();
        lines.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        lines
            .into_iter()
            .map(|(stack, count)| format!("{stack} {count}\n"))
            .collect()
    }
}

/// One entry returned by [`top_symbols`].
#[cfg(feature = "symbolize")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolCount {
    symbol: String,
    samples: u64,
}

#[cfg(feature = "symbolize")]
impl SymbolCount {
    /// Returns a best-effort dynamic symbol name or hexadecimal instruction pointer.
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Returns the number of samples attributed to the symbol.
    pub fn samples(&self) -> u64 {
        self.samples
    }
}

/// Returns the most frequently sampled symbols or raw instruction pointers.
///
/// Dynamic symbol lookup is best effort and intentionally requires no symbol
/// files or postprocessing. Unresolved addresses are formatted as hexadecimal.
#[cfg(feature = "symbolize")]
pub fn top_symbols(samples: &[Sample], limit: usize) -> Vec<SymbolCount> {
    let mut counts = HashMap::<String, u64>::new();
    for sample in samples {
        let name = dynamic_symbol(sample.ip).unwrap_or_else(|| format!("0x{:x}", sample.ip));
        *counts.entry(name).or_default() += 1;
    }
    let mut counts = counts
        .into_iter()
        .map(|(symbol, samples)| SymbolCount { symbol, samples })
        .collect::<Vec<_>>();
    counts.sort_unstable_by(|left, right| {
        right
            .samples
            .cmp(&left.samples)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
    counts.truncate(limit);
    counts
}

#[cfg(feature = "symbolize")]
fn dynamic_symbol(ip: u64) -> Option<String> {
    symbolize::current_process_symbol(ip)
}

struct Collector {
    samples: VecDeque<Sample>,
    max_samples: Option<usize>,
    dropped_samples: u64,
}

impl Collector {
    fn new(max_samples: Option<usize>) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples.unwrap_or_default()),
            max_samples,
            dropped_samples: 0,
        }
    }

    fn push(&mut self, sample: Sample) {
        if self.max_samples == Some(self.samples.len()) {
            self.samples.pop_front();
            self.dropped_samples = self.dropped_samples.saturating_add(1);
        }
        self.samples.push_back(sample);
    }

    fn take(&mut self) -> SampleBatch {
        SampleBatch {
            samples: self.samples.drain(..).collect(),
            dropped_samples: self.dropped_samples,
        }
    }
}
