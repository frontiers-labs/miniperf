use super::{derived, session};
use crate::profile;

fn kinds(jobs: &[AnalysisJob]) -> Vec<&'static str> {
    jobs.iter()
        .map(|j| match j {
            AnalysisJob::Functions { .. } => "functions",
            AnalysisJob::Allocations { .. } => "allocations",
            AnalysisJob::Scope { .. } => "scope",
            AnalysisJob::CpuLanes { .. } => "lanes",
            AnalysisJob::Balance { .. } => "balance",
            AnalysisJob::Flame { .. } => "flame",
        })
        .collect()
}
fn finish(state: &mut AnalysisState, s: &session::ShellSession, jobs: Vec<AnalysisJob>) {
    for job in jobs {
        state.install(job.compute(s));
    }
}
pub(super) fn contracts(s: &session::ShellSession) {
    use session::GlobalFilter;
    use std::sync::Arc;
    let f = GlobalFilter::default();
    // Always-hot functions; tab-only work and duplicate suppression.
    for (view, expected) in [
        (None, vec!["functions"]),
        (Some(ViewId::Summary), vec!["functions"]),
        (Some(ViewId::FlameScope), vec!["functions", "scope"]),
        (Some(ViewId::Cores), vec!["functions", "lanes", "balance"]),
        (Some(ViewId::Flame), vec!["functions", "flame"]),
    ] {
        let mut state = AnalysisState::default();
        let jobs = state.jobs(s, &f, view);
        assert_eq!(kinds(&jobs), expected);
        assert!(state.jobs(s, &f, view).is_empty());
        finish(&mut state, s, jobs);
    }
    if s.allocations.is_some() {
        let mut state = AnalysisState::default();
        let jobs = state.jobs(s, &f, Some(ViewId::Allocations));
        assert_eq!(kinds(&jobs), vec!["allocations", "functions"]);
        finish(&mut state, s, jobs);
        assert!(state.jobs(s, &f, Some(ViewId::Allocations)).is_empty());
    }
    // A time brush invalidates full-filter balance/functions but reuses spatial heatmaps.
    let mut state = AnalysisState::default();
    let jobs = state.jobs(s, &f, Some(ViewId::Cores));
    finish(&mut state, s, jobs);
    let mut brushed = f.clone();
    brushed.time = s.full_range.map(|r| profile::TimeRange {
        start_ns: r.start_ns,
        end_ns: r.start_ns + (r.end_ns - r.start_ns) / 2,
    });
    state.invalidate();
    let jobs = state.jobs(s, &brushed, Some(ViewId::Cores));
    let selected = kinds(&jobs);
    assert!(selected.contains(&"functions"));
    assert!(selected.contains(&"balance"));
    if state.cpu_lanes.latest().is_some() {
        assert!(!selected.contains(&"lanes"));
    }
    for job in &jobs {
        match job {
            AnalysisJob::Balance { filter, .. } | AnalysisJob::Functions { filter, .. } => {
                assert_eq!(*filter, brushed)
            }
            AnalysisJob::CpuLanes { filter, .. } => assert_eq!(*filter, brushed.spatial()),
            _ => panic!("unexpected job"),
        }
    }
    let mut scope = AnalysisState::default();
    let jobs = scope.jobs(s, &f, Some(ViewId::FlameScope));
    finish(&mut scope, s, jobs);
    scope.invalidate();
    let jobs = scope.jobs(s, &brushed, Some(ViewId::FlameScope));
    if scope.scope.latest().is_some() {
        assert_eq!(kinds(&jobs), vec!["functions"]);
    }
    // Existing values remain visible while a replacement computes. Superseded work is dropped.
    let mut state = AnalysisState::default();
    let first = state.jobs(s, &f, None).pop().unwrap();
    assert!(state.install(first.compute(s)));
    let old = state.functions.latest().unwrap().clone();
    state.invalidate();
    let obsolete = state.jobs(s, &brushed, None).pop().unwrap();
    assert!(Arc::ptr_eq(state.functions.latest().unwrap(), &old));
    state.invalidate();
    let current = state.jobs(s, &f, None).pop().unwrap();
    assert!(!state.install(obsolete.compute(s)));
    assert!(state.functions.is_computing());
    assert!(state.install(current.compute(s)));
    assert!(!state.functions.is_computing());
    assert!(!state.functions.stale(state.key()));
    // None clears only the current request and permits retry without losing its stale value.
    let value = Arc::new(FunctionAnalysis::build(&s.profile, &f.resolve(s)));
    let mut d = derived::Derived::default();
    d.begin(10);
    assert!(d.complete(10, Some(value.clone())));
    d.begin(11);
    assert!(!d.complete(10, None));
    assert!(d.is_computing());
    assert!(!d.complete(11, None));
    assert!(!d.is_computing());
    assert!(d.needs(11));
    assert!(Arc::ptr_eq(d.latest().unwrap(), &value));
    // Exercise production optional completion and retry for every optional cache.
    for view in [ViewId::FlameScope, ViewId::Cores] {
        let mut a = AnalysisState::default();
        let jobs = a.jobs(s, &f, Some(view));
        for job in jobs {
            match job {
                AnalysisJob::Scope { key, .. } => {
                    assert!(!a.install(AnalysisResult::Scope { key, value: None }))
                }
                AnalysisJob::CpuLanes { key, .. } => {
                    assert!(!a.install(AnalysisResult::CpuLanes { key, value: None }))
                }
                AnalysisJob::Balance { key, .. } => {
                    assert!(!a.install(AnalysisResult::Balance { key, value: None }))
                }
                j => {
                    a.install(j.compute(s));
                }
            }
        }
        let retry = a.jobs(s, &f, Some(view));
        assert_eq!(
            kinds(&retry),
            if view == ViewId::Cores {
                vec!["lanes", "balance"]
            } else {
                vec!["scope"]
            }
        );
    }
    // Keys distinguish both flame settings and preserve instruction fallback.
    let mut a = AnalysisState::default();
    a.invalidate();
    for (stacks, inverted) in [(StackMode::TopDown, false), (StackMode::BottomUp, true)] {
        for (weight, instructions) in [
            (FlameWeight::Cycles, false),
            (FlameWeight::Instructions, true),
        ] {
            a.flame_stacks = stacks;
            a.flame_weight = weight;
            a.flame_zoom = None;
            let jobs = a.jobs(s, &f, Some(ViewId::Flame));
            let job = jobs
                .into_iter()
                .find(|j| matches!(j, AnalysisJob::Flame { .. }))
                .unwrap();
            if let AnalysisJob::Flame {
                key,
                filter,
                inverted: actual,
                weight,
            } = &job
            {
                assert_eq!(*key, 4 + u64::from(inverted) * 2 + u64::from(instructions));
                assert_eq!(*actual, inverted);
                assert_eq!(*filter, f);
                assert_eq!(
                    *weight,
                    if instructions {
                        s.instructions_metric
                            .map(StackWeight::Counter)
                            .unwrap_or(StackWeight::Samples)
                    } else {
                        StackWeight::Samples
                    }
                );
            }
            assert!(a.install(job.compute(s)));
            assert_eq!(
                a.flame_layout.as_deref(),
                Some(&a.flame.latest().unwrap().icicle_layout(None))
            );
        }
    }
    a.zoom_flame(Some(0));
    let current_layout = a.flame_layout.clone();
    assert!(!a.set_flame_stacks(StackMode::BottomUp));
    assert_eq!(a.flame_zoom, Some(0));
    assert!(!a.set_flame_weight(FlameWeight::Instructions));
    assert_eq!(a.flame_zoom, Some(0));
    assert!(a.set_flame_stacks(StackMode::TopDown));
    assert_eq!(a.flame_zoom, None);
    assert_eq!(a.flame_layout, current_layout);
    a.zoom_flame(Some(0));
    assert!(a.set_flame_weight(FlameWeight::Cycles));
    assert_eq!(a.flame_zoom, None);
    assert_eq!(a.flame_layout, current_layout);
    a.set_flame_stacks(StackMode::BottomUp);
    a.set_flame_weight(FlameWeight::Instructions);
    a.zoom_flame(Some(0));
    assert_eq!(
        a.flame_layout.as_deref(),
        Some(&a.flame.latest().unwrap().icicle_layout(Some(0)))
    );
    let layout = a.flame_layout.clone();
    a.invalidate();
    assert!(a.flame_zoom.is_none());
    assert_eq!(a.flame_layout, layout);
    let generation = a.key();
    a.flame_zoom = Some(0);
    a.reset();
    assert_eq!(a.key(), generation);
    assert_eq!(a.flame_stacks, StackMode::BottomUp);
    assert_eq!(a.flame_weight, FlameWeight::Instructions);
    assert_eq!(a.flame_zoom, Some(0));
    assert!(a.flame_layout.is_none());
    assert!(a.functions.latest().is_none());
    assert!(a.flame.latest().is_none());
    assert!(a.scope.latest().is_none());
    assert!(a.cpu_lanes.latest().is_none());
    assert!(a.balance.latest().is_none());
    assert!(a.allocation_sites.latest().is_none());
    eprintln!("scheduler contract passed: {}", s.name);
}
