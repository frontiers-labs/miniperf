use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

use crate::profile::{
    CounterMetric, CpuObservation, CpuObservationSource, ProfileData, ProfileFrame, ProfileSample,
    TimeRange,
};

const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// Never fold into fewer rows than this, however sparse the recording is.
const MIN_FOLD_BINS: u64 = 12;

/// A common sample filter used by every profile analysis.
///
/// Time ranges are half-open. `required_counter` filters by counter presence
/// and a non-zero finite value, but every accepted row still contributes one
/// sample. Counter deltas are never treated as sample weights.
#[derive(Debug, Default)]
pub(crate) struct SampleFilter {
    pub range: Option<TimeRange>,
    pub process_id: Option<u32>,
    pub thread_id: Option<u32>,
    /// Accept only samples from these threads; `None` accepts every thread.
    pub threads: Option<BTreeSet<u32>>,
    pub cpu: Option<u32>,
    pub required_counter: Option<usize>,
    /// Accept a sample when its stack contains at least one selected frame.
    pub frame_ids: Option<BTreeSet<usize>>,
    /// Accept a sample when its leaf frame is in this set (module scoping).
    pub leaf_frames: Option<BTreeSet<usize>>,
}

impl SampleFilter {
    pub fn matches(&self, sample: &ProfileSample) -> bool {
        if let Some(range) = &self.range
            && (sample.timestamp_ns < range.start_ns || sample.timestamp_ns >= range.end_ns)
        {
            return false;
        }
        self.matches_without_time(sample)
    }

    fn matches_without_time(&self, sample: &ProfileSample) -> bool {
        if self
            .process_id
            .is_some_and(|process_id| sample.process_id != process_id)
        {
            return false;
        }
        if self
            .thread_id
            .is_some_and(|thread_id| sample.thread_id != thread_id)
        {
            return false;
        }
        if self
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&sample.thread_id))
        {
            return false;
        }
        if self.cpu.is_some_and(|cpu| sample.cpu != Some(cpu)) {
            return false;
        }
        if self.frame_ids.as_ref().is_some_and(|frame_ids| {
            !sample
                .stack
                .iter()
                .any(|frame_id| frame_ids.contains(frame_id))
        }) {
            return false;
        }
        if self.leaf_frames.as_ref().is_some_and(|leaf_frames| {
            !sample
                .stack
                .last()
                .is_some_and(|frame_id| leaf_frames.contains(frame_id))
        }) {
            return false;
        }
        if let Some(counter) = self.required_counter {
            return sample
                .counters
                .get(counter)
                .and_then(|value| *value)
                .is_some_and(|value| value.is_finite() && value != 0.0);
        }
        true
    }

    fn matches_cpu_observation_without_time(&self, observation: &CpuObservation) -> bool {
        if self
            .process_id
            .is_some_and(|process_id| observation.process_id != process_id)
        {
            return false;
        }
        if self
            .thread_id
            .is_some_and(|thread_id| observation.thread_id != thread_id)
        {
            return false;
        }
        if self
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&observation.thread_id))
        {
            return false;
        }
        if self.cpu.is_some_and(|cpu| observation.cpu != Some(cpu)) {
            return false;
        }
        if self.frame_ids.as_ref().is_some_and(|frame_ids| {
            !observation
                .stack
                .iter()
                .any(|frame_id| frame_ids.contains(frame_id))
        }) {
            return false;
        }
        if self.leaf_frames.as_ref().is_some_and(|leaf_frames| {
            !observation
                .stack
                .last()
                .is_some_and(|frame_id| leaf_frames.contains(frame_id))
        }) {
            return false;
        }
        true
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlameScopeBin {
    /// Number of accepted sampling observations in this cell.
    pub samples: u64,
}

/// A FlameScope-style folded heatmap.
///
/// Rows are consecutive seconds of the selected recording range and columns
/// are adaptive sub-second buckets. The grid stores sample density, not PMU
/// counter deltas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlameScopeHeatmap {
    pub range: TimeRange,
    /// Time folded into one row (classically one second, shortened for short
    /// recordings so the grid keeps a usable number of columns).
    pub fold_ns: u64,
    pub bin_width_ns: u64,
    pub rows: usize,
    pub columns: usize,
    pub bins: Vec<FlameScopeBin>,
    pub total_samples: u64,
    pub max_samples: u64,
}

impl FlameScopeHeatmap {
    pub fn build(profile: &ProfileData, filter: &SampleFilter, max_bins: usize) -> Option<Self> {
        let range = analysis_range(profile, filter)?;
        let duration = range.end_ns.saturating_sub(range.start_ns);
        if duration == 0 {
            return None;
        }

        let fold_ns = fold_period(duration);
        let rows = usize::try_from(div_ceil(duration, fold_ns))
            .unwrap_or(usize::MAX)
            .max(1);

        // Bin height follows sample density, not just time: a grid finer than
        // the samples can fill reads as noise, not as a heatmap.
        let accepted = profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
            .filter(|sample| {
                sample.timestamp_ns >= range.start_ns && sample.timestamp_ns < range.end_ns
            })
            .count() as u64;
        let cap = max_bins.max(1) as u64;
        let target_bins = (accepted / rows as u64 / 2).clamp(MIN_FOLD_BINS.min(cap), cap);
        let bin_width_ns = nice_bin_width(div_ceil(fold_ns, target_bins))
            .min(fold_ns)
            .max(1);
        let columns = usize::try_from(div_ceil(fold_ns, bin_width_ns))
            .unwrap_or(usize::MAX)
            .max(1);
        let cell_count = rows.checked_mul(columns)?;
        let mut bins = vec![FlameScopeBin::default(); cell_count];
        let mut total_samples = 0u64;

        for sample in profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
        {
            if sample.timestamp_ns < range.start_ns || sample.timestamp_ns >= range.end_ns {
                continue;
            }
            let relative = sample.timestamp_ns - range.start_ns;
            let row = usize::try_from(relative / fold_ns).ok()?;
            let within_fold = relative % fold_ns;
            let column = usize::try_from(within_fold / bin_width_ns).ok()?;
            let Some(bin) = row
                .checked_mul(columns)
                .and_then(|index| index.checked_add(column))
                .and_then(|index| bins.get_mut(index))
            else {
                continue;
            };
            bin.samples = bin.samples.saturating_add(1);
            total_samples = total_samples.saturating_add(1);
        }

        let max_samples = bins.iter().map(|bin| bin.samples).max().unwrap_or(0);
        Some(Self {
            range,
            fold_ns,
            bin_width_ns,
            rows,
            columns,
            bins,
            total_samples,
            max_samples,
        })
    }

    pub fn bin(&self, row: usize, column: usize) -> Option<&FlameScopeBin> {
        row.checked_mul(self.columns)
            .and_then(|index| index.checked_add(column))
            .and_then(|index| self.bins.get(index))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallTreeNode {
    pub id: usize,
    pub frame_id: Option<usize>,
    pub label: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: usize,
    pub inclusive_samples: u64,
    pub self_samples: u64,
}

/// A deterministic top-down call tree. Each accepted profile row has weight one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallTree {
    pub nodes: Vec<CallTreeNode>,
    pub root: usize,
    pub total_samples: u64,
}

/// How much one accepted sample contributes to a call-tree node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackWeight {
    /// Every sample weighs one — the cycles-proportional default.
    Samples,
    /// The sample's value for `counter_metrics[index]`, rounded down.
    Counter(usize),
}

impl StackWeight {
    fn of(self, sample: &ProfileSample) -> u64 {
        match self {
            Self::Samples => 1,
            Self::Counter(index) => sample
                .counters
                .get(index)
                .copied()
                .flatten()
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value as u64)
                .unwrap_or(0),
        }
    }
}

impl CallTree {
    /// `inverted` reverses every stack, turning the tree bottom-up (leaves at
    /// the root); `weight` scales each sample's contribution.
    pub fn build_weighted(
        profile: &ProfileData,
        filter: &SampleFilter,
        inverted: bool,
        weight: StackWeight,
    ) -> Self {
        let labels = frame_labels(&profile.frames);
        let mut samples = profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
            .collect::<Vec<_>>();
        samples.sort_by(|left, right| {
            (
                left.timestamp_ns,
                left.process_id,
                left.thread_id,
                left.cpu,
                &left.stack,
            )
                .cmp(&(
                    right.timestamp_ns,
                    right.process_id,
                    right.thread_id,
                    right.cpu,
                    &right.stack,
                ))
        });

        let mut nodes = vec![CallTreeNode {
            id: 0,
            frame_id: None,
            label: "All samples".to_owned(),
            parent: None,
            children: Vec::new(),
            depth: 0,
            inclusive_samples: 0,
            self_samples: 0,
        }];
        let mut child_lookup = BTreeMap::<(usize, usize), usize>::new();

        for sample in samples {
            let value = weight.of(sample);
            if value == 0 {
                continue;
            }
            nodes[0].inclusive_samples = nodes[0].inclusive_samples.saturating_add(value);
            let mut parent = 0usize;
            let stack: Vec<usize> = if inverted {
                sample.stack.iter().rev().copied().collect()
            } else {
                sample.stack.clone()
            };
            for (depth, frame_id) in stack.into_iter().enumerate() {
                let node_id = if let Some(node_id) = child_lookup.get(&(parent, frame_id)) {
                    *node_id
                } else {
                    let node_id = nodes.len();
                    nodes.push(CallTreeNode {
                        id: node_id,
                        frame_id: Some(frame_id),
                        label: labels
                            .get(&frame_id)
                            .cloned()
                            .unwrap_or_else(|| format!("Unknown frame {frame_id}")),
                        parent: Some(parent),
                        children: Vec::new(),
                        depth: depth + 1,
                        inclusive_samples: 0,
                        self_samples: 0,
                    });
                    nodes[parent].children.push(node_id);
                    child_lookup.insert((parent, frame_id), node_id);
                    node_id
                };
                nodes[node_id].inclusive_samples =
                    nodes[node_id].inclusive_samples.saturating_add(value);
                parent = node_id;
            }
            nodes[parent].self_samples = nodes[parent].self_samples.saturating_add(value);
        }

        for node_id in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[node_id].children);
            children.sort_by(|left, right| {
                Reverse(nodes[*left].inclusive_samples)
                    .cmp(&Reverse(nodes[*right].inclusive_samples))
                    .then_with(|| nodes[*left].label.cmp(&nodes[*right].label))
                    .then_with(|| nodes[*left].frame_id.cmp(&nodes[*right].frame_id))
            });
            nodes[node_id].children = children;
        }

        let total_samples = nodes[0].inclusive_samples;
        Self {
            nodes,
            root: 0,
            total_samples,
        }
    }

    pub fn focus_path(&self, focus_node: usize) -> Vec<usize> {
        let mut path = Vec::new();
        let mut current = self.nodes.get(focus_node).map(|node| node.id);
        while let Some(node_id) = current {
            path.push(node_id);
            current = self.nodes[node_id].parent;
        }
        path.reverse();
        path
    }

    pub fn icicle_layout(&self, focus_node: Option<usize>) -> IcicleLayout {
        let focus_node = focus_node
            .filter(|node| *node < self.nodes.len())
            .unwrap_or(self.root);
        let total_samples = self.nodes[focus_node].inclusive_samples;
        let mut frames = Vec::new();
        let mut max_depth = 0usize;
        if total_samples > 0 {
            self.layout_node(focus_node, 0.0, 1.0, 0, &mut max_depth, &mut frames);
        }
        IcicleLayout {
            focus_node,
            focus_path: self.focus_path(focus_node),
            total_samples,
            max_depth,
            frames,
        }
    }

    fn layout_node(
        &self,
        node_id: usize,
        x: f64,
        width: f64,
        depth: usize,
        max_depth: &mut usize,
        frames: &mut Vec<IcicleFrame>,
    ) {
        let node = &self.nodes[node_id];
        *max_depth = (*max_depth).max(depth);
        frames.push(IcicleFrame {
            node_id,
            frame_id: node.frame_id,
            label: node.label.clone(),
            x,
            width,
            depth,
            inclusive_samples: node.inclusive_samples,
            self_samples: node.self_samples,
        });

        if node.inclusive_samples == 0 {
            return;
        }
        let mut child_x = x;
        for child_id in &node.children {
            let child = &self.nodes[*child_id];
            let child_width =
                width * child.inclusive_samples as f64 / node.inclusive_samples as f64;
            self.layout_node(
                *child_id,
                child_x,
                child_width,
                depth + 1,
                max_depth,
                frames,
            );
            child_x += child_width;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IcicleFrame {
    pub node_id: usize,
    pub frame_id: Option<usize>,
    pub label: String,
    pub x: f64,
    pub width: f64,
    pub depth: usize,
    pub inclusive_samples: u64,
    pub self_samples: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IcicleLayout {
    pub focus_node: usize,
    pub focus_path: Vec<usize>,
    pub total_samples: u64,
    pub max_depth: usize,
    pub frames: Vec<IcicleFrame>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionStat {
    pub frame_id: usize,
    pub label: String,
    pub inclusive_samples: u64,
    pub self_samples: u64,
    pub inclusive_fraction: f64,
    pub self_fraction: f64,
    pub metrics: FunctionMetrics,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FunctionMetrics {
    pub cpu_time_ns: Option<f64>,
    pub cycles: Option<f64>,
    pub instructions: Option<f64>,
    pub ipc: Option<f64>,
    pub llc_miss_rate: Option<f64>,
    pub llc_mpki: Option<f64>,
    pub backend_stall_fraction: Option<f64>,
    pub branch_mpki: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionRelation {
    pub frame_id: usize,
    pub label: String,
    pub samples: u64,
    pub fraction_of_function: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionDetails {
    pub function: FunctionStat,
    pub callers: Vec<FunctionRelation>,
    pub callees: Vec<FunctionRelation>,
}

/// Function-level self/inclusive sample counts and direct call-edge counts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionAnalysis {
    pub total_samples: u64,
    pub functions: Vec<FunctionStat>,
    labels: BTreeMap<usize, String>,
    edges: BTreeMap<(usize, usize), u64>,
}

impl FunctionAnalysis {
    pub fn build(profile: &ProfileData, filter: &SampleFilter) -> Self {
        let labels = frame_labels(&profile.frames);
        let confidence_scaled = profile
            .counter_metrics
            .iter()
            .map(|metric| confidence_scaled_metric(&metric.key))
            .collect::<Vec<_>>();
        let mut inclusive = BTreeMap::<usize, u64>::new();
        let mut self_counts = BTreeMap::<usize, u64>::new();
        let mut edges = BTreeMap::<(usize, usize), u64>::new();
        let mut counter_sums = BTreeMap::<usize, Vec<Option<f64>>>::new();
        let mut total_samples = 0u64;
        let mut unique_frames = Vec::new();
        let mut unique_edges = Vec::new();

        for sample in profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
        {
            total_samples = total_samples.saturating_add(1);
            unique_frames.clear();
            unique_frames.extend_from_slice(&sample.stack);
            unique_frames.sort_unstable();
            unique_frames.dedup();
            for frame_id in &unique_frames {
                *inclusive.entry(*frame_id).or_default() += 1;
            }
            if let Some(frame_id) = sample.stack.last() {
                *self_counts.entry(*frame_id).or_default() += 1;
                let sums = counter_sums
                    .entry(*frame_id)
                    .or_insert_with(|| vec![None; profile.counter_metrics.len()]);
                for ((sum, value), scaled) in sums
                    .iter_mut()
                    .zip(&sample.counters)
                    .zip(&confidence_scaled)
                {
                    if let Some(value) = value.filter(|value| value.is_finite()) {
                        let value = if *scaled {
                            value / sample.confidence
                        } else {
                            value
                        };
                        *sum = Some(sum.unwrap_or_default() + value);
                    }
                }
            }
            unique_edges.clear();
            unique_edges.extend(sample.stack.windows(2).map(|frames| (frames[0], frames[1])));
            unique_edges.sort_unstable();
            unique_edges.dedup();
            for edge in &unique_edges {
                *edges.entry(*edge).or_default() += 1;
            }
        }

        let metric_indices = MetricIndices::resolve(&profile.counter_metrics);
        let mut functions = inclusive
            .into_iter()
            .map(|(frame_id, inclusive_samples)| {
                let self_samples = self_counts.get(&frame_id).copied().unwrap_or(0);
                FunctionStat {
                    frame_id,
                    label: labels
                        .get(&frame_id)
                        .cloned()
                        .unwrap_or_else(|| format!("Unknown frame {frame_id}")),
                    inclusive_samples,
                    self_samples,
                    inclusive_fraction: fraction(inclusive_samples, total_samples),
                    self_fraction: fraction(self_samples, total_samples),
                    metrics: function_metrics(
                        counter_sums.get(&frame_id).map(Vec::as_slice),
                        metric_indices,
                    ),
                }
            })
            .collect::<Vec<_>>();
        functions.sort_by(|left, right| {
            Reverse(left.inclusive_samples)
                .cmp(&Reverse(right.inclusive_samples))
                .then_with(|| Reverse(left.self_samples).cmp(&Reverse(right.self_samples)))
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.frame_id.cmp(&right.frame_id))
        });

        Self {
            total_samples,
            functions,
            labels,
            edges,
        }
    }

    pub fn details(&self, frame_id: usize) -> Option<FunctionDetails> {
        let function = self
            .functions
            .iter()
            .find(|function| function.frame_id == frame_id)?
            .clone();
        let denominator = function.inclusive_samples;
        let mut callers = self
            .edges
            .iter()
            .filter(|((_, callee), _)| *callee == frame_id)
            .map(|((caller, _), samples)| self.relation(*caller, *samples, denominator))
            .collect::<Vec<_>>();
        let mut callees = self
            .edges
            .iter()
            .filter(|((caller, _), _)| *caller == frame_id)
            .map(|((_, callee), samples)| self.relation(*callee, *samples, denominator))
            .collect::<Vec<_>>();
        sort_relations(&mut callers);
        sort_relations(&mut callees);
        Some(FunctionDetails {
            function,
            callers,
            callees,
        })
    }

    fn relation(&self, frame_id: usize, samples: u64, denominator: u64) -> FunctionRelation {
        FunctionRelation {
            frame_id,
            label: self
                .labels
                .get(&frame_id)
                .cloned()
                .unwrap_or_else(|| format!("Unknown frame {frame_id}")),
            samples,
            fraction_of_function: fraction(samples, denominator),
        }
    }
}

/// Counter column positions, resolved once per analysis instead of per function.
#[derive(Clone, Copy, Debug, Default)]
struct MetricIndices {
    cpu_time: Option<usize>,
    cycles: Option<usize>,
    instructions: Option<usize>,
    llc_misses: Option<usize>,
    llc_references: Option<usize>,
    backend_stalls: Option<usize>,
    branch_misses: Option<usize>,
}

impl MetricIndices {
    fn resolve(metrics: &[CounterMetric]) -> Self {
        Self {
            cpu_time: find_metric(metrics, &["os_cpu_clock", "cpu_clock"]),
            cycles: find_metric(metrics, &["cycles", "pmu_cycles"]),
            instructions: find_metric(metrics, &["instructions", "pmu_instructions"]),
            llc_misses: find_metric(metrics, &["llc_misses", "pmu_llc_misses"]),
            llc_references: find_metric(metrics, &["llc_references", "pmu_llc_references"]),
            backend_stalls: find_metric(
                metrics,
                &["stalled_cycles_backend", "pmu_stalled_cycles_backend"],
            ),
            branch_misses: find_metric(metrics, &["branch_misses", "pmu_branch_misses"]),
        }
    }
}

fn function_metrics(sums: Option<&[Option<f64>]>, indices: MetricIndices) -> FunctionMetrics {
    let Some(sums) = sums else {
        return FunctionMetrics::default();
    };
    let value = |index: Option<usize>| index.and_then(|index| sums.get(index).copied().flatten());
    let cycles = value(indices.cycles);
    let instructions = value(indices.instructions);
    let llc_misses = value(indices.llc_misses);
    let llc_references = value(indices.llc_references);
    let backend_stalls = value(indices.backend_stalls);
    let branch_misses = value(indices.branch_misses);
    let ratio = |numerator: Option<f64>, denominator: Option<f64>| {
        numerator
            .zip(denominator)
            .filter(|(_, denominator)| *denominator > 0.0)
            .map(|(numerator, denominator)| numerator / denominator)
            .filter(|value| value.is_finite())
    };

    FunctionMetrics {
        cpu_time_ns: value(indices.cpu_time),
        cycles,
        instructions,
        ipc: ratio(instructions, cycles),
        llc_miss_rate: ratio(
            llc_misses,
            llc_misses
                .zip(llc_references)
                .map(|(misses, references)| misses + references),
        ),
        llc_mpki: ratio(llc_misses.map(|value| value * 1_000.0), instructions),
        backend_stall_fraction: ratio(backend_stalls, cycles),
        branch_mpki: ratio(branch_misses.map(|value| value * 1_000.0), instructions),
    }
}

fn confidence_scaled_metric(key: &str) -> bool {
    let key = normalized_metric_key(key);
    key.contains("llc")
        || key.contains("branch")
        || key.contains("stalledcycles")
        || key.contains("cachemiss")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TimelineLaneKey {
    Cpu(Option<u32>),
    Thread(u32),
}

impl TimelineLaneKey {
    pub fn label(self) -> String {
        match self {
            Self::Cpu(Some(cpu)) => format!("CPU {cpu}"),
            Self::Cpu(None) => "CPU unknown".to_owned(),
            Self::Thread(thread_id) => format!("Thread {thread_id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CpuUtilizationBucket {
    /// Attributed CPU-clock nanoseconds before utilization is capped.
    pub cpu_time_ns: f64,
    /// `cpu_time_ns / wall_bucket_duration`, capped to the physical [0, 1] range.
    pub utilization: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CpuUtilizationLane {
    pub key: TimelineLaneKey,
    pub label: String,
    pub buckets: Vec<CpuUtilizationBucket>,
}

/// CPU-clock utilization or sampled occupancy aligned to wall-time buckets.
///
/// Counter deltas are split over their preceding wall-time interval. Sampled
/// occupancy observations are point-attributed to the CPU reported by that
/// timer hit, avoiding false back-projection across CPU migrations.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CpuUtilizationHeatmap {
    pub range: TimeRange,
    pub bucket_duration_ns: u64,
    pub buckets: usize,
    pub uses_cpu_lanes: bool,
    pub source: CpuObservationSource,
    pub lanes: Vec<CpuUtilizationLane>,
}

impl CpuUtilizationHeatmap {
    pub fn build_with_bucket_duration(
        profile: &ProfileData,
        filter: &SampleFilter,
        max_buckets: usize,
        bucket_duration_ns: Option<u64>,
    ) -> Option<Self> {
        let range = analysis_range(profile, filter)?;
        let duration = range.end_ns.saturating_sub(range.start_ns);
        if duration == 0 {
            return None;
        }
        let bucket_duration_ns = match bucket_duration_ns {
            Some(0) => return None,
            Some(duration) => duration,
            None => nice_bin_width(div_ceil(duration, max_buckets.max(1) as u64)).max(1),
        };
        let buckets = usize::try_from(div_ceil(duration, bucket_duration_ns))
            .unwrap_or(usize::MAX)
            .max(1);

        let eligible = profile
            .cpu_observations
            .iter()
            .filter(|observation| filter.matches_cpu_observation_without_time(observation))
            .filter(|observation| {
                if observation.weight_ns == 0 {
                    return false;
                }
                match observation.source {
                    CpuObservationSource::SampledOccupancy => {
                        observation.timestamp_ns >= range.start_ns
                            && observation.timestamp_ns < range.end_ns
                    }
                    CpuObservationSource::CounterDelta | CpuObservationSource::LegacyUnknown => {
                        observation
                            .wall_interval_start_ns()
                            .is_some_and(|interval_start| {
                                interval_start < observation.timestamp_ns
                                    && interval_start < range.end_ns
                                    && observation.timestamp_ns > range.start_ns
                            })
                    }
                }
            })
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return None;
        }
        let source = eligible
            .iter()
            .map(|observation| observation.source)
            .reduce(|left, right| {
                if left == right {
                    left
                } else {
                    CpuObservationSource::LegacyUnknown
                }
            })
            .unwrap_or(CpuObservationSource::LegacyUnknown);
        let uses_cpu_lanes =
            !eligible.is_empty() && eligible.iter().all(|observation| observation.cpu.is_some());
        let mut lane_buckets = BTreeMap::<TimelineLaneKey, Vec<f64>>::new();
        if uses_cpu_lanes && let Some(logical_cpu_count) = profile.logical_cpu_count {
            for cpu in 0..logical_cpu_count {
                lane_buckets.insert(TimelineLaneKey::Cpu(Some(cpu)), vec![0.0; buckets]);
            }
        }

        for observation in eligible {
            let key = if uses_cpu_lanes {
                TimelineLaneKey::Cpu(observation.cpu)
            } else {
                TimelineLaneKey::Thread(observation.thread_id)
            };
            let attributed = lane_buckets
                .entry(key)
                .or_insert_with(|| vec![0.0; buckets]);

            if observation.source == CpuObservationSource::SampledOccupancy {
                let bucket = usize::try_from(
                    (observation.timestamp_ns - range.start_ns) / bucket_duration_ns,
                )
                .unwrap_or(usize::MAX)
                .min(buckets - 1);
                attributed[bucket] += observation.weight_ns as f64;
                continue;
            }

            let interval_end = observation.timestamp_ns;
            let Some(interval_start) = observation.wall_interval_start_ns() else {
                continue;
            };
            let interval_duration = interval_end.saturating_sub(interval_start);
            if interval_duration == 0 {
                continue;
            }
            let clipped_start = interval_start.max(range.start_ns);
            let clipped_end = interval_end.min(range.end_ns);
            if clipped_start >= clipped_end {
                continue;
            }
            let first_bucket =
                usize::try_from((clipped_start - range.start_ns) / bucket_duration_ns)
                    .unwrap_or(usize::MAX)
                    .min(buckets - 1);
            let last_bucket =
                usize::try_from((clipped_end - 1 - range.start_ns) / bucket_duration_ns)
                    .unwrap_or(usize::MAX)
                    .min(buckets - 1);
            for (bucket, value) in attributed
                .iter_mut()
                .enumerate()
                .take(last_bucket + 1)
                .skip(first_bucket)
            {
                let bucket_start = range
                    .start_ns
                    .saturating_add((bucket as u64).saturating_mul(bucket_duration_ns));
                let bucket_end = bucket_start
                    .saturating_add(bucket_duration_ns)
                    .min(range.end_ns);
                let overlap_start = clipped_start.max(bucket_start);
                let overlap_end = clipped_end.min(bucket_end);
                if overlap_start < overlap_end {
                    let overlap_ns = overlap_end - overlap_start;
                    *value +=
                        observation.weight_ns as f64 * overlap_ns as f64 / interval_duration as f64;
                }
            }
        }

        if lane_buckets.is_empty() {
            return None;
        }
        let lanes = lane_buckets
            .into_iter()
            .map(|(key, attributed)| CpuUtilizationLane {
                key,
                label: key.label(),
                buckets: attributed
                    .into_iter()
                    .enumerate()
                    .map(|(bucket, cpu_time_ns)| {
                        let bucket_start = range
                            .start_ns
                            .saturating_add((bucket as u64).saturating_mul(bucket_duration_ns));
                        let bucket_end = bucket_start
                            .saturating_add(bucket_duration_ns)
                            .min(range.end_ns);
                        let wall_duration = bucket_end.saturating_sub(bucket_start);
                        let utilization = if wall_duration == 0 {
                            0.0
                        } else {
                            (cpu_time_ns / wall_duration as f64).clamp(0.0, 1.0)
                        };
                        CpuUtilizationBucket {
                            cpu_time_ns,
                            utilization,
                        }
                    })
                    .collect(),
            })
            .collect();

        Some(Self {
            range,
            bucket_duration_ns,
            buckets,
            uses_cpu_lanes,
            source,
            lanes,
        })
    }
}

/// Elapsed time spent with N CPUs simultaneously busy, folded out of the same
/// occupancy attribution the per-CPU lanes are painted from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConcurrencyHistogram {
    /// Seconds spent at index-many busy CPUs; slot 0 is fully idle time.
    pub slots: Vec<f64>,
    pub average_busy: f64,
    pub total_seconds: f64,
}

impl ConcurrencyHistogram {
    pub fn build(heatmap: &CpuUtilizationHeatmap, logical_cpu_count: Option<u32>) -> Self {
        let cpus = logical_cpu_count
            .map(|count| count as usize)
            .unwrap_or(0)
            .max(heatmap.lanes.len())
            .max(1);
        let mut slots = vec![0.0; cpus + 1];
        let mut weighted = 0.0;
        let mut total_seconds = 0.0;
        for bucket in 0..heatmap.buckets {
            let start = heatmap
                .range
                .start_ns
                .saturating_add((bucket as u64).saturating_mul(heatmap.bucket_duration_ns));
            let end = start
                .saturating_add(heatmap.bucket_duration_ns)
                .min(heatmap.range.end_ns);
            let seconds = end.saturating_sub(start) as f64 / NANOS_PER_SECOND as f64;
            if seconds <= 0.0 {
                continue;
            }
            let busy: f64 = heatmap
                .lanes
                .iter()
                .filter_map(|lane| lane.buckets.get(bucket))
                .map(|bucket| bucket.utilization)
                .sum();
            let slot = (busy.round().max(0.0) as usize).min(cpus);
            slots[slot] += seconds;
            weighted += slot as f64 * seconds;
            total_seconds += seconds;
        }
        Self {
            slots,
            average_busy: if total_seconds > 0.0 {
                weighted / total_seconds
            } else {
                0.0
            },
            total_seconds,
        }
    }
}

/// Frames that mean "this thread is waiting on someone else". Matched as
/// substrings because libc spells them differently on every platform.
const SYNC_FRAME_MARKERS: [&str; 12] = [
    "futex",
    "barrier",
    "pthread_cond",
    "pthread_join",
    "lll_lock",
    "sem_wait",
    "psynch",
    "ulock_wait",
    "cond_wait",
    "sched_yield",
    "nanosleep",
    "mutex_lock",
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThreadBalanceRow {
    pub thread_id: u32,
    /// Attributed CPU time over the analysed wall time.
    pub busy_fraction: f64,
    /// Share of the thread's samples parked in a synchronization frame.
    pub sync_fraction: f64,
    pub migrations: u64,
    pub samples: u64,
}

/// Per-thread busy/wait/migration balance over the filtered range.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThreadBalance {
    pub rows: Vec<ThreadBalanceRow>,
    /// Whether any observation carried a CPU id; migrations mean nothing without.
    pub has_cpu_ids: bool,
}

impl ThreadBalance {
    pub fn build(profile: &ProfileData, filter: &SampleFilter) -> Option<Self> {
        let range = analysis_range(profile, filter)?;
        let wall = range.end_ns.saturating_sub(range.start_ns) as f64;
        if wall <= 0.0 {
            return None;
        }
        let sync_frames: BTreeSet<usize> = profile
            .frames
            .iter()
            .filter(|frame| {
                let name = frame.name.to_lowercase();
                SYNC_FRAME_MARKERS
                    .iter()
                    .any(|marker| name.contains(marker))
            })
            .map(|frame| frame.id)
            .collect();

        let mut samples = BTreeMap::<u32, (u64, u64)>::new();
        let mut cpu_time = BTreeMap::<u32, f64>::new();
        let mut trails = BTreeMap::<u32, Vec<(u64, u32)>>::new();
        let mut has_cpu_ids = false;

        for sample in profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
        {
            let entry = samples.entry(sample.thread_id).or_default();
            entry.0 += 1;
            if sample.stack.iter().any(|frame| sync_frames.contains(frame)) {
                entry.1 += 1;
            }
            if let Some(cpu) = sample.cpu {
                has_cpu_ids = true;
                trails
                    .entry(sample.thread_id)
                    .or_default()
                    .push((sample.timestamp_ns, cpu));
            }
        }

        for observation in profile
            .cpu_observations
            .iter()
            .filter(|observation| filter.matches_cpu_observation_without_time(observation))
            .filter(|observation| {
                observation.timestamp_ns >= range.start_ns
                    && observation.timestamp_ns < range.end_ns
            })
        {
            *cpu_time.entry(observation.thread_id).or_default() += observation.weight_ns as f64;
            if let Some(cpu) = observation.cpu {
                has_cpu_ids = true;
                trails
                    .entry(observation.thread_id)
                    .or_default()
                    .push((observation.timestamp_ns, cpu));
            }
        }

        let threads: BTreeSet<u32> = samples.keys().chain(cpu_time.keys()).copied().collect();
        let rows = threads
            .into_iter()
            .map(|thread_id| {
                let (total, sync) = samples.get(&thread_id).copied().unwrap_or_default();
                ThreadBalanceRow {
                    thread_id,
                    busy_fraction: (cpu_time.get(&thread_id).copied().unwrap_or(0.0) / wall)
                        .clamp(0.0, 1.0),
                    sync_fraction: fraction(sync, total),
                    migrations: migrations(trails.get_mut(&thread_id)),
                    samples: total,
                }
            })
            .collect();
        Some(Self { rows, has_cpu_ids })
    }
}

/// Counts CPU changes along a thread's time-ordered trail of observations.
fn migrations(trail: Option<&mut Vec<(u64, u32)>>) -> u64 {
    let Some(trail) = trail else {
        return 0;
    };
    trail.sort_unstable();
    trail
        .windows(2)
        .filter(|pair| pair[0].1 != pair[1].1)
        .count() as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CounterAggregation {
    Sum,
    Ratio,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CounterTrack {
    pub key: String,
    pub label: String,
    pub aggregation: CounterAggregation,
    /// Values aligned to `CounterTracks::bins`; missing data remains `None`.
    pub values: Vec<Option<f64>>,
}

/// Time-aligned sums for recorded counters plus a sum(instructions)/sum(cycles)
/// IPC track when both source counters exist.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CounterTracks {
    pub range: TimeRange,
    pub bin_width_ns: u64,
    pub bins: usize,
    pub sample_counts: Vec<u64>,
    pub tracks: Vec<CounterTrack>,
}

impl CounterTracks {
    pub fn build(profile: &ProfileData, filter: &SampleFilter, max_bins: usize) -> Option<Self> {
        let range = analysis_range(profile, filter)?;
        let duration = range.end_ns.saturating_sub(range.start_ns);
        if duration == 0 {
            return None;
        }
        let bin_width_ns = nice_bin_width(div_ceil(duration, max_bins.max(1) as u64)).max(1);
        let bins = usize::try_from(div_ceil(duration, bin_width_ns))
            .unwrap_or(usize::MAX)
            .max(1);
        let metric_count = profile.counter_metrics.len();
        let mut sums = vec![vec![0.0f64; bins]; metric_count];
        let mut present = vec![vec![false; bins]; metric_count];
        let mut sample_counts = vec![0u64; bins];

        for sample in profile
            .samples
            .iter()
            .filter(|sample| filter.matches(sample))
        {
            if sample.timestamp_ns < range.start_ns || sample.timestamp_ns >= range.end_ns {
                continue;
            }
            let bin = usize::try_from((sample.timestamp_ns - range.start_ns) / bin_width_ns)
                .unwrap_or(usize::MAX)
                .min(bins - 1);
            sample_counts[bin] = sample_counts[bin].saturating_add(1);
            for (metric_index, value) in sample.counters.iter().enumerate().take(metric_count) {
                let Some(value) = value.filter(|value| value.is_finite()) else {
                    continue;
                };
                sums[metric_index][bin] += value;
                present[metric_index][bin] = true;
            }
        }

        let mut tracks = profile
            .counter_metrics
            .iter()
            .enumerate()
            .map(|(index, metric)| CounterTrack {
                key: metric.key.clone(),
                label: metric.label.clone(),
                aggregation: CounterAggregation::Sum,
                values: (0..bins)
                    .map(|bin| present[index][bin].then_some(sums[index][bin]))
                    .collect(),
            })
            .collect::<Vec<_>>();

        if let (Some(cycles), Some(instructions)) = (
            find_metric(&profile.counter_metrics, &["cycles", "pmu_cycles"]),
            find_metric(
                &profile.counter_metrics,
                &["instructions", "pmu_instructions"],
            ),
        ) {
            tracks.push(CounterTrack {
                key: "derived.ipc".to_owned(),
                label: "IPC (derived)".to_owned(),
                aggregation: CounterAggregation::Ratio,
                values: (0..bins)
                    .map(|bin| {
                        (present[cycles][bin]
                            && present[instructions][bin]
                            && sums[cycles][bin] != 0.0)
                            .then_some(sums[instructions][bin] / sums[cycles][bin])
                            .filter(|value| value.is_finite())
                    })
                    .collect(),
            });
        }

        Some(Self {
            range,
            bin_width_ns,
            bins,
            sample_counts,
            tracks,
        })
    }
}

fn analysis_range(profile: &ProfileData, filter: &SampleFilter) -> Option<TimeRange> {
    let full = profile.full_range()?;
    let range = if let Some(filtered) = &filter.range {
        TimeRange {
            start_ns: full.start_ns.max(filtered.start_ns),
            end_ns: full.end_ns.min(filtered.end_ns),
        }
    } else {
        full
    };
    (range.start_ns < range.end_ns).then_some(range)
}

fn frame_labels(frames: &[ProfileFrame]) -> BTreeMap<usize, String> {
    frames
        .iter()
        .map(|frame| (frame.id, frame.name.clone()))
        .collect()
}

fn sort_relations(relations: &mut [FunctionRelation]) {
    relations.sort_by(|left, right| {
        Reverse(left.samples)
            .cmp(&Reverse(right.samples))
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.frame_id.cmp(&right.frame_id))
    });
}

/// Column of the retired-instructions counter, when the recording has one.
pub(crate) fn instructions_metric(metrics: &[CounterMetric]) -> Option<usize> {
    find_metric(metrics, &["instructions", "pmu_instructions"])
}

fn find_metric(metrics: &[CounterMetric], candidates: &[&str]) -> Option<usize> {
    metrics.iter().position(|metric| {
        let key = normalized_metric_key(&metric.key);
        candidates
            .iter()
            .any(|candidate| key == normalized_metric_key(candidate))
    })
}

fn normalized_metric_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn fraction(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return u64::MAX;
    }
    value / divisor + u64::from(!value.is_multiple_of(divisor))
}

/// Returns the next 1/2/5 × 10ⁿ interval at or above `minimum`.
/// Time folded into one flame-scope row. One second is the classic choice and
/// stays the cap; shorter recordings fold tighter so the grid keeps roughly
/// sixty columns instead of collapsing into two.
fn fold_period(duration_ns: u64) -> u64 {
    nice_bin_width(div_ceil(duration_ns, 60).max(1))
}

fn nice_bin_width(minimum: u64) -> u64 {
    let minimum = minimum.max(1);
    let mut magnitude = 1u64;
    while magnitude <= minimum / 10 {
        let next = magnitude.saturating_mul(10);
        if next == magnitude {
            break;
        }
        magnitude = next;
    }
    for multiplier in [1u64, 2, 5, 10] {
        let candidate = magnitude.saturating_mul(multiplier);
        if candidate >= minimum {
            return candidate;
        }
    }
    u64::MAX
}
