use super::{analysis::*, session};
use crate::profile;
use crate::profile_analysis::{CallTree, FunctionAnalysis, StackWeight, ThreadBalance};

fn kinds(results: &[AnalysisResult]) -> Vec<&'static str> {
    results
        .iter()
        .map(|j| match j {
            AnalysisResult::Functions { .. } => "functions",
            AnalysisResult::Allocations { .. } => "allocations",
            AnalysisResult::Scope { .. } => "scope",
            AnalysisResult::CpuLanes { .. } => "lanes",
            AnalysisResult::Balance { .. } => "balance",
            AnalysisResult::Flame { .. } => "flame",
        })
        .collect()
}
fn compute(s: &session::ShellSession, jobs: Vec<AnalysisJob>) -> Vec<AnalysisResult> {
    jobs.into_iter().map(|job| job(s)).collect()
}
fn finish(state: &mut AnalysisState, results: Vec<AnalysisResult>) {
    for result in results {
        state.install(result);
    }
}
pub(super) fn contracts(s: &session::ShellSession) {
    use session::GlobalFilter;
    let f = GlobalFilter::default();
    // Always-hot functions; tab-only work and duplicate suppression.
    for (view, expected) in [
        (AnalysisDemand::Functions, vec!["functions"]),
        (AnalysisDemand::Scope, vec!["functions", "scope"]),
        (AnalysisDemand::Cores, vec!["functions", "lanes", "balance"]),
        (AnalysisDemand::Flame, vec!["functions", "flame"]),
    ] {
        let mut state = AnalysisState::default();
        let results = compute(s, state.jobs(s, &f, view));
        assert_eq!(kinds(&results), expected);
        assert!(state.jobs(s, &f, view).is_empty());
        finish(&mut state, results);
    }
    if s.allocations.is_some() {
        let mut state = AnalysisState::default();
        let results = compute(s, state.jobs(s, &f, AnalysisDemand::Allocations));
        assert_eq!(kinds(&results), vec!["allocations", "functions"]);
        finish(&mut state, results);
        assert!(state.jobs(s, &f, AnalysisDemand::Allocations).is_empty());
    }
    // A time brush invalidates full-filter balance/functions but reuses spatial heatmaps.
    let mut state = AnalysisState::default();
    let results = compute(s, state.jobs(s, &f, AnalysisDemand::Cores));
    finish(&mut state, results);
    let mut brushed = f.clone();
    brushed.time = s.full_range.map(|r| profile::TimeRange {
        start_ns: r.start_ns,
        end_ns: r.start_ns + (r.end_ns - r.start_ns) / 2,
    });
    state.invalidate();
    let results = compute(s, state.jobs(s, &brushed, AnalysisDemand::Cores));
    let selected = kinds(&results);
    assert!(selected.contains(&"functions"));
    assert!(selected.contains(&"balance"));
    if state.cpu_lanes.latest().is_some() {
        assert!(!selected.contains(&"lanes"));
    }
    finish(&mut state, results);
    assert_eq!(
        state.functions.latest().map(|value| value.as_ref()),
        Some(&FunctionAnalysis::build(&s.profile, &brushed.resolve(s)))
    );
    assert_eq!(
        state.balance.latest().map(|value| value.as_ref()),
        ThreadBalance::build(&s.profile, &brushed.resolve(s)).as_ref()
    );
    let mut scope = AnalysisState::default();
    let results = compute(s, scope.jobs(s, &f, AnalysisDemand::Scope));
    finish(&mut scope, results);
    scope.invalidate();
    let results = compute(s, scope.jobs(s, &brushed, AnalysisDemand::Scope));
    if scope.scope.latest().is_some() {
        assert_eq!(kinds(&results), vec!["functions"]);
    }
    let mut fresh_scope = AnalysisState::default();
    let results = compute(s, fresh_scope.jobs(s, &brushed, AnalysisDemand::Scope));
    finish(&mut fresh_scope, results);
    assert_eq!(fresh_scope.scope.latest(), scope.scope.latest());
    let mut fresh_lanes = AnalysisState::default();
    let results = compute(s, fresh_lanes.jobs(s, &brushed, AnalysisDemand::Cores));
    finish(&mut fresh_lanes, results);
    assert_eq!(fresh_lanes.cpu_lanes.latest(), state.cpu_lanes.latest());
    // Existing values remain visible while a replacement computes. Superseded work is dropped.
    let mut state = AnalysisState::default();
    let first = state.jobs(s, &f, AnalysisDemand::Functions).pop().unwrap();
    assert!(state.install(first(s)));
    let old = state.functions.latest().unwrap().clone();
    state.invalidate();
    let obsolete = state
        .jobs(s, &brushed, AnalysisDemand::Functions)
        .pop()
        .unwrap();
    assert_eq!(state.functions.latest().unwrap().as_ref(), old.as_ref());
    state.invalidate();
    let current = state.jobs(s, &f, AnalysisDemand::Functions).pop().unwrap();
    assert!(!state.install(obsolete(s)));
    assert!(state.functions_stale());
    assert!(state.jobs(s, &f, AnalysisDemand::Functions).is_empty());
    assert!(state.install(current(s)));
    assert!(state.jobs(s, &f, AnalysisDemand::Functions).is_empty());
    assert!(!state.functions_stale());
    // An absent replacement keeps the installed optional value and permits retry.
    for view in [AnalysisDemand::Scope, AnalysisDemand::Cores] {
        let mut a = AnalysisState::default();
        let results = compute(s, a.jobs(s, &f, view));
        finish(&mut a, results);
        let mut changed = f.clone();
        if let Some(thread) = s.threads.first() {
            changed.disabled_threads.insert(thread.thread_id);
        } else {
            changed.symbol = "no recorded symbol".into();
        }
        a.invalidate();
        for job in compute(s, a.jobs(s, &changed, view)) {
            match job {
                AnalysisResult::Scope { key, .. } => {
                    let old = a.scope.latest().cloned();
                    assert!(!a.install(AnalysisResult::Scope { key, value: None }));
                    assert_eq!(a.scope.latest(), old.as_ref());
                }
                AnalysisResult::CpuLanes { key, .. } => {
                    let old = a.cpu_lanes.latest().cloned();
                    assert!(!a.install(AnalysisResult::CpuLanes { key, value: None }));
                    assert_eq!(a.cpu_lanes.latest(), old.as_ref());
                }
                AnalysisResult::Balance { key, .. } => {
                    let old = a.balance.latest().cloned();
                    assert!(!a.install(AnalysisResult::Balance { key, value: None }));
                    assert_eq!(a.balance.latest(), old.as_ref());
                }
                j => {
                    a.install(j);
                }
            }
        }
        let retry = compute(s, a.jobs(s, &changed, view));
        assert_eq!(
            kinds(&retry),
            if view == AnalysisDemand::Cores {
                vec!["lanes", "balance"]
            } else {
                vec!["scope"]
            }
        );
    }
    // Flame options request new work and preserve instruction fallback.
    let mut a = AnalysisState::default();
    a.invalidate();
    let mut expected_tree = None;
    for (stacks, inverted) in [(StackMode::TopDown, false), (StackMode::BottomUp, true)] {
        for (weight, instructions) in [
            (FlameWeight::Cycles, false),
            (FlameWeight::Instructions, true),
        ] {
            a.set_flame_stacks(stacks);
            a.set_flame_weight(weight);
            a.zoom_flame(None);
            let results = compute(s, a.jobs(s, &f, AnalysisDemand::Flame));
            let job = results
                .into_iter()
                .find(|j| matches!(j, AnalysisResult::Flame { .. }))
                .unwrap();
            let expected_weight = if instructions {
                s.instructions_metric
                    .map(StackWeight::Counter)
                    .unwrap_or(StackWeight::Samples)
            } else {
                StackWeight::Samples
            };
            expected_tree = Some(CallTree::build_weighted(
                &s.profile,
                &f.resolve(s),
                inverted,
                expected_weight,
            ));
            assert!(a.jobs(s, &f, AnalysisDemand::Flame).is_empty());
            assert!(a.install(job));
            assert_eq!(
                a.flame_layout.as_deref(),
                Some(&expected_tree.as_ref().unwrap().icicle_layout(None))
            );
        }
    }
    a.zoom_flame(Some(0));
    let current_layout = a.flame_layout.clone();
    assert!(!a.set_flame_stacks(StackMode::BottomUp));
    assert!(a.flame_zoom.is_some());
    assert!(!a.set_flame_weight(FlameWeight::Instructions));
    assert!(a.flame_zoom.is_some());
    assert!(a.set_flame_stacks(StackMode::TopDown));
    assert!(a.flame_zoom.is_none());
    assert_eq!(a.flame_layout.clone(), current_layout);
    a.zoom_flame(Some(0));
    assert!(a.set_flame_weight(FlameWeight::Cycles));
    assert!(a.flame_zoom.is_none());
    assert_eq!(a.flame_layout.clone(), current_layout);
    a.set_flame_stacks(StackMode::BottomUp);
    a.set_flame_weight(FlameWeight::Instructions);
    a.zoom_flame(Some(0));
    assert_eq!(
        a.flame_layout.as_deref(),
        Some(&expected_tree.as_ref().unwrap().icicle_layout(Some(0)))
    );
    let layout = a.flame_layout.clone();
    a.invalidate();
    assert!(a.flame_zoom.is_none());
    assert_eq!(a.flame_layout.clone(), layout);
    a.zoom_flame(Some(0));
    a.reset();
    assert_eq!(a.flame_stacks, StackMode::BottomUp);
    assert_eq!(a.flame_weight, FlameWeight::Instructions);
    assert!(a.flame_layout.is_none());
    assert!(a.functions.latest().is_none());
    assert!(a.scope.latest().is_none());
    assert!(a.cpu_lanes.latest().is_none());
    assert!(a.balance.latest().is_none());
    assert!(a.allocation_sites.latest().is_none());
    assert_eq!(
        kinds(&compute(s, a.jobs(s, &f, AnalysisDemand::Flame))),
        vec!["functions", "flame"]
    );
    eprintln!("scheduler contract passed: {}", s.name);
}

pub(super) fn recording_reset_contract(
    old: &session::ShellSession,
    new: &session::ShellSession,
) -> [bool; 2] {
    let filter = session::GlobalFilter::default();
    let mut exercised = [false; 2];
    for (index, demand) in [AnalysisDemand::Scope, AnalysisDemand::Cores]
        .into_iter()
        .enumerate()
    {
        let mut state = AnalysisState::default();
        let old_results = compute(old, state.jobs(old, &filter, demand))
            .into_iter()
            .filter(|result| {
                matches!(
                    result,
                    AnalysisResult::Scope { .. } | AnalysisResult::CpuLanes { .. }
                )
            })
            .collect::<Vec<_>>();
        let had_value = old_results.iter().any(|result| match result {
            AnalysisResult::Scope { value, .. } => value.is_some(),
            AnalysisResult::CpuLanes { value, .. } => value.is_some(),
            _ => false,
        });
        if !had_value {
            continue;
        }
        exercised[index] = true;
        state.reset();
        state.invalidate();
        let current = state.jobs(new, &filter, demand);
        for result in old_results {
            assert!(
                !state.install(result),
                "old recording installed after reset: {} -> {} ({demand:?})",
                old.name,
                new.name
            );
        }
        assert!(state.scope.latest().is_none());
        assert!(state.cpu_lanes.latest().is_none());
        for job in current {
            let result = job(new);
            match result {
                AnalysisResult::Scope { key, value } => {
                    let installed = value.is_some();
                    let expected = value.clone();
                    assert_eq!(
                        state.install(AnalysisResult::Scope { key, value }),
                        installed
                    );
                    assert_eq!(state.scope.latest(), expected.as_ref());
                }
                AnalysisResult::CpuLanes { key, value } => {
                    let installed = value.is_some();
                    let expected = value.clone();
                    assert_eq!(
                        state.install(AnalysisResult::CpuLanes { key, value }),
                        installed
                    );
                    assert_eq!(state.cpu_lanes.latest(), expected.as_ref());
                }
                other => {
                    state.install(other);
                }
            }
        }
        eprintln!(
            "delayed recording result rejected: {} -> {} ({demand:?}, old value present: {had_value})",
            old.name, new.name
        );
    }
    exercised
}

pub(crate) fn check(paths: &[String]) {
    let mut previous = None;
    let mut exercised = [false; 2];
    for path in paths {
        let current = session::ShellSession::load(std::path::Path::new(path))
            .expect("load profiler recording");
        contracts(&current);
        let pair = recording_reset_contract(previous.as_ref().unwrap_or(&current), &current);
        for (covered, ran) in exercised.iter_mut().zip(pair) {
            *covered |= ran;
        }
        previous = Some(current);
    }
    assert!(
        exercised.into_iter().all(|ran| ran),
        "recordings must exercise nonempty delayed Scope and Cores results"
    );
}
