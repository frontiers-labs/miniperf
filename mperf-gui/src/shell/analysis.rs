//! Derived datasets and their background work, independent of GPUI scheduling.

use std::{collections::HashMap, sync::Arc};

use super::{
    derived::Derived,
    session::{GlobalFilter, ShellSession},
};
use crate::allocations::Boundary;
use crate::profile_analysis::{
    CallTree, CpuUtilizationHeatmap, FlameScopeHeatmap, FunctionAnalysis, IcicleLayout,
    SampleFilter, StackWeight, ThreadBalance,
};

#[derive(Default)]
pub(super) struct Site {
    pub(super) title: String,
    pub(super) count: u64,
    pub(super) requested: u64,
    pub(super) live: u64,
    pub(super) ended_lifetime: u64,
    pub(super) ended_count: u64,
    pub(super) peak: u64,
}

fn allocation_sites(session: &ShellSession, filter: &SampleFilter) -> Vec<(u64, Site)> {
    let data = session.allocations.as_ref().unwrap();
    let cutoff = filter.range.map(|range| range.end_ns).unwrap_or(u64::MAX);
    let mut sites = HashMap::<u64, (Site, Vec<(Boundary, i64)>)>::new();
    for lifetime in &data.lifetimes {
        let stack = data.stacks.get(&lifetime.stack);
        if filter.range.is_some_and(|range| {
            lifetime.start.time < range.start_ns || lifetime.start.time >= range.end_ns
        }) || filter
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&lifetime.tid))
            || filter.regions.as_ref().is_some_and(|regions| {
                !regions.contains(lifetime.pid, lifetime.tid, lifetime.start.time)
            })
            || filter.frame_ids.as_ref().is_some_and(|selected| {
                !stack.is_some_and(|frames| frames.iter().any(|id| selected.contains(id)))
            })
            || filter.leaf_frames.as_ref().is_some_and(|selected| {
                !data
                    .application_frames
                    .get(&lifetime.stack)
                    .is_some_and(|frame| selected.contains(frame))
            })
        {
            continue;
        }
        let (site, changes) = sites.entry(lifetime.stack).or_default();
        site.count += 1;
        site.requested += lifetime.bytes;
        changes.push((lifetime.start, lifetime.bytes as i64));
        if let Some(end) = lifetime.end.filter(|end| end.time <= cutoff) {
            site.ended_lifetime += end.time.saturating_sub(lifetime.start.time);
            site.ended_count += 1;
            changes.push((end, -(lifetime.bytes as i64)));
        } else {
            site.live += lifetime.bytes;
        }
    }
    let mut sites = sites
        .into_iter()
        .map(|(stack, (mut site, mut changes))| {
            changes.sort_unstable_by_key(|(boundary, _)| *boundary);
            let mut live = 0i64;
            let mut peak = 0i64;
            for (_, delta) in changes {
                live += delta;
                peak = peak.max(live);
            }
            site.peak = peak as u64;
            site.title = data.application_frames.get(&stack).map_or_else(
                || "[unknown stack]".into(),
                |frame| session.profile.frames[*frame].friendly_name.clone(),
            );
            (stack, site)
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|(_, site)| std::cmp::Reverse(site.requested));
    sites
}

/// Upper bound on flame-scope rows per fold; the real count follows density.
const SCOPE_MAX_BINS: usize = 50;
/// Time buckets in the Cores occupancy lanes.
const CORE_LANE_BUCKETS: usize = 360;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum StackMode {
    #[default]
    TopDown,
    BottomUp,
}

/// Flame width semantics. Instructions are offered only with a recorded counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum FlameWeight {
    #[default]
    Cycles,
    Instructions,
}

/// Extra analysis requested by the active view. Functions are always available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AnalysisDemand {
    #[default]
    Functions,
    Allocations,
    Scope,
    Cores,
    Flame,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpatialKey {
    recording_epoch: u64,
    selection: GlobalFilter,
}

#[derive(Default)]
pub(super) struct AnalysisState {
    generation: u64,
    recording_epoch: u64,
    functions: Derived<FunctionAnalysis>,
    flame: Derived<CallTree>,
    scope: Derived<FlameScopeHeatmap, SpatialKey>,
    cpu_lanes: Derived<CpuUtilizationHeatmap, SpatialKey>,
    balance: Derived<ThreadBalance>,
    allocation_sites: Derived<Vec<(u64, Site)>>,
    flame_stacks: StackMode,
    flame_weight: FlameWeight,
    flame_zoom: Option<usize>,
    flame_layout: Option<Arc<IcicleLayout>>,
}

impl AnalysisState {
    fn key(&self) -> u64 {
        self.generation
    }

    pub fn functions(&self) -> Option<&Arc<FunctionAnalysis>> {
        self.functions.latest()
    }

    pub fn functions_stale(&self) -> bool {
        self.functions.stale(&self.key())
    }

    pub fn allocation_sites(&self) -> Option<&Arc<Vec<(u64, Site)>>> {
        self.allocation_sites.latest()
    }

    pub fn allocation_sites_stale(&self) -> bool {
        self.allocation_sites.stale(&self.key())
    }

    pub fn scope(&self) -> Option<&Arc<FlameScopeHeatmap>> {
        self.scope.latest()
    }

    pub fn cpu_lanes(&self) -> Option<&Arc<CpuUtilizationHeatmap>> {
        self.cpu_lanes.latest()
    }

    pub fn cpu_lanes_computing(&self) -> bool {
        self.cpu_lanes.is_computing()
    }

    pub fn balance(&self) -> Option<&Arc<ThreadBalance>> {
        self.balance.latest()
    }

    pub fn balance_computing(&self) -> bool {
        self.balance.is_computing()
    }

    pub fn flame_layout(&self) -> Option<&Arc<IcicleLayout>> {
        self.flame_layout.as_ref()
    }

    pub fn flame_zoomed(&self) -> bool {
        self.flame_zoom.is_some()
    }

    pub fn flame_stacks(&self) -> StackMode {
        self.flame_stacks
    }

    pub fn set_flame_stacks(&mut self, stacks: StackMode) -> bool {
        if self.flame_stacks == stacks {
            return false;
        }
        self.flame_stacks = stacks;
        self.flame_zoom = None;
        true
    }

    pub fn flame_weight(&self) -> FlameWeight {
        self.flame_weight
    }

    pub fn set_flame_weight(&mut self, weight: FlameWeight) -> bool {
        if self.flame_weight == weight {
            return false;
        }
        self.flame_weight = weight;
        self.flame_zoom = None;
        true
    }

    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.flame_zoom = None;
    }

    pub fn reset(&mut self) {
        self.recording_epoch += 1;
        self.functions.reset();
        self.flame.reset();
        self.scope.reset();
        self.cpu_lanes.reset();
        self.balance.reset();
        self.allocation_sites.reset();
        self.flame_layout = None;
    }

    pub fn zoom_flame(&mut self, node: Option<usize>) {
        self.flame_zoom = node;
        self.rebuild_flame_layout();
    }

    fn rebuild_flame_layout(&mut self) {
        self.flame_layout = self
            .flame
            .latest()
            .map(|tree| Arc::new(tree.icicle_layout(self.flame_zoom)));
    }

    /// Hotspots feed the selection panel and status bar. Other datasets are lazy.
    pub fn jobs(
        &mut self,
        session: &ShellSession,
        filter: &GlobalFilter,
        demand: AnalysisDemand,
    ) -> Vec<AnalysisJob> {
        let key = self.key();
        let mut jobs = Vec::new();
        if demand == AnalysisDemand::Allocations && self.allocation_sites.needs(&key) {
            self.allocation_sites.begin(key);
            jobs.push(AnalysisJob::Allocations {
                key,
                filter: filter.clone(),
            });
        }
        if self.functions.needs(&key) {
            self.functions.begin(key);
            jobs.push(AnalysisJob::Functions {
                key,
                filter: filter.clone(),
            });
        }
        match demand {
            AnalysisDemand::Scope => {
                let key = SpatialKey {
                    recording_epoch: self.recording_epoch,
                    selection: filter.spatial(),
                };
                if self.scope.needs(&key) {
                    self.scope.begin(key.clone());
                    jobs.push(AnalysisJob::Scope {
                        key,
                        filter: filter.spatial(),
                    });
                }
            }
            AnalysisDemand::Cores => {
                let spatial_key = SpatialKey {
                    recording_epoch: self.recording_epoch,
                    selection: filter.spatial(),
                };
                if self.cpu_lanes.needs(&spatial_key) {
                    self.cpu_lanes.begin(spatial_key.clone());
                    jobs.push(AnalysisJob::CpuLanes {
                        key: spatial_key,
                        filter: filter.spatial(),
                    });
                }
                if self.balance.needs(&key) {
                    self.balance.begin(key);
                    jobs.push(AnalysisJob::Balance {
                        key,
                        filter: filter.clone(),
                    });
                }
            }
            AnalysisDemand::Flame => {
                let inverted = self.flame_stacks == StackMode::BottomUp;
                let instructions = self.flame_weight == FlameWeight::Instructions;
                let key = key * 4 + u64::from(inverted) * 2 + u64::from(instructions);
                if self.flame.needs(&key) {
                    self.flame.begin(key);
                    let weight = if instructions {
                        session
                            .instructions_metric
                            .map(StackWeight::Counter)
                            .unwrap_or(StackWeight::Samples)
                    } else {
                        StackWeight::Samples
                    };
                    jobs.push(AnalysisJob::Flame {
                        key,
                        filter: filter.clone(),
                        inverted,
                        weight,
                    });
                }
            }
            _ => {}
        }
        jobs
    }

    /// Returns whether the completed work changed what a view can render.
    pub fn install(&mut self, result: AnalysisResult) -> bool {
        match result {
            AnalysisResult::Functions { key, value } => self.functions.complete(key, Some(value)),
            AnalysisResult::Allocations { key, value } => {
                self.allocation_sites.complete(key, Some(value))
            }
            AnalysisResult::Scope { key, value } => self.scope.complete(key, value),
            AnalysisResult::CpuLanes { key, value } => self.cpu_lanes.complete(key, value),
            AnalysisResult::Balance { key, value } => self.balance.complete(key, value),
            AnalysisResult::Flame { key, value } => {
                if !self.flame.complete(key, Some(value)) {
                    return false;
                }
                self.rebuild_flame_layout();
                true
            }
        }
    }
}

pub(super) enum AnalysisJob {
    Functions {
        key: u64,
        filter: GlobalFilter,
    },
    Allocations {
        key: u64,
        filter: GlobalFilter,
    },
    Scope {
        key: SpatialKey,
        filter: GlobalFilter,
    },
    CpuLanes {
        key: SpatialKey,
        filter: GlobalFilter,
    },
    Balance {
        key: u64,
        filter: GlobalFilter,
    },
    Flame {
        key: u64,
        filter: GlobalFilter,
        inverted: bool,
        weight: StackWeight,
    },
}

pub(super) enum AnalysisResult {
    Functions {
        key: u64,
        value: Arc<FunctionAnalysis>,
    },
    Allocations {
        key: u64,
        value: Arc<Vec<(u64, Site)>>,
    },
    Scope {
        key: SpatialKey,
        value: Option<Arc<FlameScopeHeatmap>>,
    },
    CpuLanes {
        key: SpatialKey,
        value: Option<Arc<CpuUtilizationHeatmap>>,
    },
    Balance {
        key: u64,
        value: Option<Arc<ThreadBalance>>,
    },
    Flame {
        key: u64,
        value: Arc<CallTree>,
    },
}

impl AnalysisJob {
    pub fn compute(self, session: &ShellSession) -> AnalysisResult {
        match self {
            Self::Functions { key, filter } => AnalysisResult::Functions {
                key,
                value: Arc::new(FunctionAnalysis::build(
                    &session.profile,
                    &filter.resolve(session),
                )),
            },
            Self::Allocations { key, filter } => AnalysisResult::Allocations {
                key,
                value: Arc::new(allocation_sites(session, &filter.resolve(session))),
            },
            Self::Scope { key, filter } => AnalysisResult::Scope {
                key,
                value: FlameScopeHeatmap::build(
                    &session.profile,
                    &filter.resolve(session),
                    SCOPE_MAX_BINS,
                )
                .map(Arc::new),
            },
            Self::CpuLanes { key, filter } => AnalysisResult::CpuLanes {
                key,
                value: CpuUtilizationHeatmap::build_with_bucket_duration(
                    &session.profile,
                    &filter.resolve(session),
                    CORE_LANE_BUCKETS,
                    None,
                )
                .map(Arc::new),
            },
            Self::Balance { key, filter } => AnalysisResult::Balance {
                key,
                value: ThreadBalance::build(&session.profile, &filter.resolve(session))
                    .map(Arc::new),
            },
            Self::Flame {
                key,
                filter,
                inverted,
                weight,
            } => AnalysisResult::Flame {
                key,
                value: Arc::new(CallTree::build_weighted(
                    &session.profile,
                    &filter.resolve(session),
                    inverted,
                    weight,
                )),
            },
        }
    }
}
