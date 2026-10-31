use crate::profile_analysis::{
    CallTree, CpuUtilizationHeatmap, FlameScopeHeatmap, FunctionAnalysis, StackWeight,
    ThreadBalance,
};
use analysis::{AnalysisResult, AnalysisState, FlameWeight, StackMode};

fn filters(session: &session::ShellSession) -> Vec<(&'static str, session::GlobalFilter)> {
    use session::GlobalFilter;
    let mut result = vec![("all", GlobalFilter::default())];
    if let Some(range) = session.full_range {
        result.push((
            "half-time",
            GlobalFilter {
                time: Some(profile::TimeRange {
                    start_ns: range.start_ns,
                    end_ns: range.start_ns + (range.end_ns - range.start_ns) / 2,
                }),
                ..Default::default()
            },
        ));
    }
    if let Some(thread) = session.threads.first() {
        let mut filter = GlobalFilter::default();
        filter.disabled_threads.insert(thread.thread_id);
        result.push(("thread", filter));
    }
    if !session.modules.is_empty() {
        let mut filter = GlobalFilter::default();
        filter.disabled_modules.insert(0);
        result.push(("module", filter));
    }
    if let Some(frame) = session.profile.frames.first() {
        result.push((
            "symbol",
            GlobalFilter {
                symbol: frame.friendly_name.clone(),
                ..Default::default()
            },
        ));
    }
    if !session.regions.nodes.is_empty() {
        result.push((
            "region",
            GlobalFilter {
                region: Some(regions::RegionSelection::Node(regions::RegionNodeId(0))),
                ..Default::default()
            },
        ));
    }
    result
}

pub fn check(directory: &std::path::Path) {
    let session = session::ShellSession::load(directory).expect("load profiler recording");
    analysis::contracts(&session);
    for (_, filter) in filters(&session) {
        let full = filter.resolve(&session);
        let spatial = filter.spatial().resolve(&session);
        for view in [
            ViewId::Summary,
            ViewId::FlameScope,
            ViewId::Cores,
            ViewId::Flame,
            ViewId::Allocations,
        ] {
            if view == ViewId::Allocations && session.allocations.is_none() {
                continue;
            }
            for (stacks, inverted) in [(StackMode::TopDown, false), (StackMode::BottomUp, true)] {
                for (weight, instructions) in [
                    (FlameWeight::Cycles, false),
                    (FlameWeight::Instructions, true),
                ] {
                    let mut state = AnalysisState::default();
                    state.set_flame_stacks(stacks);
                    state.set_flame_weight(weight);
                    let jobs = state.jobs(&session, &filter, Some(view));
                    for job in jobs {
                        let result = job.compute(&session);
                        match &result {
                            AnalysisResult::Functions { key, value } => {
                                assert_eq!(*key, 0);
                                assert_eq!(
                                    value.as_ref(),
                                    &FunctionAnalysis::build(&session.profile, &full)
                                );
                            }
                            AnalysisResult::Allocations { key, value } => {
                                assert_eq!(*key, 0);
                                assert_eq!(
                                    analysis::values(value),
                                    analysis::values(&analysis::expected_allocation_sites(
                                        &session, &full
                                    ))
                                );
                            }
                            AnalysisResult::Scope { key, value } => {
                                assert_eq!(*key, filter.spatial_key());
                                assert_eq!(
                                    value.as_deref(),
                                    FlameScopeHeatmap::build(&session.profile, &spatial, 50)
                                        .as_ref()
                                );
                            }
                            AnalysisResult::CpuLanes { key, value } => {
                                assert_eq!(*key, filter.spatial_key());
                                assert_eq!(
                                    value.as_deref(),
                                    CpuUtilizationHeatmap::build_with_bucket_duration(
                                        &session.profile,
                                        &spatial,
                                        360,
                                        None
                                    )
                                    .as_ref()
                                );
                            }
                            AnalysisResult::Balance { key, value } => {
                                assert_eq!(*key, 0);
                                assert_eq!(
                                    value.as_deref(),
                                    ThreadBalance::build(&session.profile, &full).as_ref()
                                );
                            }
                            AnalysisResult::Flame { key, value } => {
                                assert_eq!(*key, u64::from(inverted) * 2 + u64::from(instructions));
                                let weight = if instructions {
                                    session
                                        .instructions_metric
                                        .map(StackWeight::Counter)
                                        .unwrap_or(StackWeight::Samples)
                                } else {
                                    StackWeight::Samples
                                };
                                let expected = CallTree::build_weighted(
                                    &session.profile,
                                    &full,
                                    inverted,
                                    weight,
                                );
                                assert_eq!(value.as_ref(), &expected);
                                assert_eq!(value.icicle_layout(None), expected.icicle_layout(None));
                            }
                        }
                        state.install(result);
                    }
                }
            }
        }
    }
    println!("source scheduler/builders agree: {}", directory.display());
}
