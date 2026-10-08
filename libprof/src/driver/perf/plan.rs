//! Packs the counters of a sampling request into perf groups for one core PMU.
//!
//! A sample reports the counters of the group that overflowed and nothing
//! else, so a formula is only computable from events that share a group. The
//! planner therefore packs by formula membership and PMU capacity. It makes no
//! syscalls: the driver describes the PMU, and opens what this returns.

use std::collections::HashSet;

use crate::cpu_family::CPUFamily;
use crate::Counter;

/// One perf event group.
#[derive(Debug, PartialEq)]
pub(crate) struct Group {
    /// Open order. The first event is the perf group leader.
    pub events: Vec<Counter>,
    /// Index of the event that owns the ring buffer. Every other event is a
    /// counting member reported through its grouped read.
    pub sampled: usize,
}

/// A set of events that must share a group and cannot on this PMU.
#[derive(Debug, PartialEq)]
pub(crate) struct TooWide {
    pub events: Vec<String>,
    /// Events a group holds here beyond cycles and instructions.
    pub available: usize,
}

/// The PMU event a counter resolves to on `family`, which is what the hardware
/// actually has to find a counter for.
fn resolved_event(counter: &Counter, family: Option<&CPUFamily>) -> String {
    let name = counter.name();
    family
        .and_then(|family| family.aliases.get(name))
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

/// Whether an event rides Intel's PERF_METRICS MSR or its fixed `slots`
/// counter, and so takes no programmable counter.
fn is_fixed_topdown(counter: &Counter) -> bool {
    matches!(counter, Counter::Custom(name)
        if name == crate::GROUP_LEADER || crate::topdown::is_perf_metrics_event(name))
}

/// Plans the sampling groups for one PMU.
///
/// `counters` are the ones this PMU can open, in any order. Each set in
/// `co_scheduled` names events one formula reads together; members that are
/// not in `counters` are ignored. `reserved_counters` are hardware counters
/// the kernel keeps for itself.
///
/// Rules, in priority order:
/// 1. One counter per PMU event. A group naming an event twice asks for two
///    counters to count the same thing, and a PMU with a fixed event-to-counter
///    map (RISC-V sscofpmf) accepts that group and then never schedules it.
/// 2. Every group starts with the family's leader event, when it is not what
///    cycles already resolves to, then cycles and instructions.
/// 3. A group holding `slots` is led by it and sampled by cycles: Intel
///    PERF_METRICS demands that leader and refuses to let it sample.
/// 4. Co-scheduled sets that share an event merge, so the event opens once and
///    both formulas read it from the same samples.
/// 5. A set never splits. One wider than a capacity the event table states is
///    an error; with no stated capacity it gets a group to itself and the
///    kernel has the last word.
/// 6. Everything else fills groups first-fit, largest set first.
/// 7. Software events join the first group only. Repeating task-clock in every
///    group duplicates the same cumulative counter.
///
/// Without cycles there is nothing for the PMU to sample on, and the request
/// becomes a single group led by `cpu-clock`.
pub(crate) fn plan(
    family: Option<&CPUFamily>,
    reserved_counters: usize,
    counters: &[Counter],
    co_scheduled: &[Vec<Counter>],
) -> Result<Vec<Group>, TooWide> {
    let mut seen = HashSet::new();
    let mut unique: Vec<Counter> = counters
        .iter()
        .filter(|counter| seen.insert(resolved_event(counter, family)))
        .cloned()
        .collect();
    if unique.is_empty() {
        return Ok(Vec::new());
    }

    if !unique.contains(&Counter::Cycles) {
        if let Some(clock) = unique.iter().position(|c| *c == Counter::CpuClock) {
            unique[..=clock].rotate_right(1);
        }
        return Ok(vec![Group {
            events: unique,
            sampled: 0,
        }]);
    }

    let leader = family
        .and_then(|family| family.leader_event.as_ref())
        .map(|leader| {
            unique
                .iter()
                .find(|counter| resolved_event(counter, family) == *leader)
                .cloned()
                .unwrap_or_else(|| Counter::Custom(leader.clone()))
        })
        .filter(|leader| *leader != Counter::Cycles);

    let base: Vec<Counter> = leader
        .iter()
        .cloned()
        .chain([Counter::Cycles])
        .chain(
            unique
                .contains(&Counter::Instructions)
                .then_some(Counter::Instructions),
        )
        .collect();
    let (software, hardware): (Vec<Counter>, Vec<Counter>) = unique
        .into_iter()
        .filter(|counter| !base.contains(counter))
        .partition(Counter::is_software);

    let stated = family.and_then(|family| family.max_counters);
    let capacity = stated
        .unwrap_or(if leader.is_some() { 2 } else { 3 })
        .saturating_sub(reserved_counters)
        .max(1);

    // Union the hardware events that formulas tie together. `set_of[i]` is the
    // lowest index of the set event `i` belongs to.
    let mut set_of: Vec<usize> = (0..hardware.len()).collect();
    for set in co_scheduled {
        let members: Vec<usize> = set
            .iter()
            .filter_map(|counter| {
                let event = resolved_event(counter, family);
                hardware
                    .iter()
                    .position(|candidate| resolved_event(candidate, family) == event)
            })
            .collect();
        let Some(target) = members.iter().map(|member| set_of[*member]).min() else {
            continue;
        };
        for member in members {
            let merged = set_of[member];
            for slot in &mut set_of {
                if *slot == merged {
                    *slot = target;
                }
            }
        }
    }

    let mut sets: Vec<(usize, Vec<usize>)> = Vec::new();
    for index in 0..hardware.len() {
        if set_of[index] != index {
            continue;
        }
        let members: Vec<usize> = (index..hardware.len())
            .filter(|member| set_of[*member] == index)
            .collect();
        let cost = members
            .iter()
            .filter(|member| !is_fixed_topdown(&hardware[**member]))
            .count();
        if cost > capacity && stated.is_some() {
            return Err(TooWide {
                events: members
                    .iter()
                    .map(|member| hardware[*member].name().to_owned())
                    .collect(),
                available: capacity,
            });
        }
        sets.push((cost, members));
    }
    // Stable, so sets of one size keep the order they were requested in.
    sets.sort_by_key(|(cost, _)| std::cmp::Reverse(*cost));

    let mut bins: Vec<(usize, Vec<usize>)> = Vec::new();
    for (cost, members) in sets {
        match bins.iter_mut().find(|(used, _)| used + cost <= capacity) {
            Some((used, bin)) => {
                *used += cost;
                bin.extend(members);
            }
            None => bins.push((cost, members)),
        }
    }
    if bins.is_empty() {
        bins.push((0, Vec::new()));
    }

    let mut software = Some(software);
    Ok(bins
        .into_iter()
        .map(|(_, mut members)| {
            members.sort_unstable();
            let mut events = base.clone();
            events.extend(members.into_iter().map(|member| hardware[member].clone()));
            events.extend(software.take().unwrap_or_default());

            let slots = events.iter().position(
                |event| matches!(event, Counter::Custom(name) if name == crate::GROUP_LEADER),
            );
            let sampled = match slots {
                Some(slots) => {
                    events[..=slots].rotate_right(1);
                    events
                        .iter()
                        .position(|event| *event == Counter::Cycles)
                        .unwrap_or(0)
                }
                None => 0,
            };
            Group { events, sampled }
        })
        .collect())
}

/// The per-group sampling frequency to ask for, given how many sampling groups
/// a plan opens on one CPU.
///
/// Every group has its own sampling leader, so `groups` leaders at `requested`
/// Hz ask the CPU for `groups * requested` interrupts a second. Past
/// `perf_event_max_sample_rate` the kernel throttles, and throttling stops the
/// whole group while its `time_running` keeps accruing: the counters then
/// accumulate almost nothing and the recording is quietly off by orders of
/// magnitude rather than merely sparse. Measured on a 48-core Zen with a
/// 1000 Hz cap, a TMA recording asking 3x1000 Hz reported 4.3M cycles for a
/// 3-second run that actually retired 8.8G.
///
/// Four fifths of the ceiling leaves room for the other sampling events a
/// scenario may open, such as precise memory sampling.
pub(crate) fn group_sample_freq(requested: u64, groups: usize, max_rate: Option<u64>) -> u64 {
    let Some(max_rate) = max_rate else {
        return requested;
    };
    let groups = groups.max(1) as u64;
    let budget = (max_rate * 4 / 5 / groups).max(1);
    requested.min(budget)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn family(
        max_counters: Option<usize>,
        leader_event: Option<&str>,
        aliases: &[(&str, &str)],
    ) -> CPUFamily {
        CPUFamily {
            name: "Test".to_owned(),
            vendor: "Test".to_owned(),
            id: "test".to_owned(),
            max_counters,
            leader_event: leader_event.map(str::to_owned),
            events: Default::default(),
            aliases: aliases
                .iter()
                .map(|(target, origin)| ((*target).to_owned(), (*origin).to_owned()))
                .collect(),
            metrics: Vec::new(),
            scenarios: Default::default(),
        }
    }

    fn custom(name: &str) -> Counter {
        Counter::Custom(name.to_owned())
    }

    fn customs(names: &[&str]) -> Vec<Counter> {
        names.iter().map(|name| custom(name)).collect()
    }

    fn request(names: &[&str]) -> Vec<Counter> {
        [Counter::Cycles, Counter::Instructions]
            .into_iter()
            .chain(customs(names))
            .collect()
    }

    /// The group holding `event`, as an index into `groups`.
    fn holder(groups: &[Group], event: &Counter) -> usize {
        let holders: Vec<usize> = groups
            .iter()
            .enumerate()
            .filter_map(|(index, group)| group.events.contains(event).then_some(index))
            .collect();
        assert_eq!(holders.len(), 1, "{event:?} must open exactly once");
        holders[0]
    }

    #[test]
    fn a_formula_stays_in_one_group_whatever_the_request_order() {
        let family = family(Some(2), None, &[]);
        // Positional chunks of two would pair a with x and y with b.
        let counters = request(&["a", "x", "y", "b"]);
        let groups = plan(Some(&family), 0, &counters, &[customs(&["a", "b"])]).unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(holder(&groups, &custom("a")), holder(&groups, &custom("b")));
        assert_eq!(holder(&groups, &custom("x")), holder(&groups, &custom("y")));
    }

    #[test]
    fn formulas_sharing_an_event_merge_into_one_group() {
        let family = family(Some(3), None, &[]);
        let counters = request(&["a", "b", "c"]);
        let sets = [customs(&["a", "b"]), customs(&["b", "c"])];
        let groups = plan(Some(&family), 0, &counters, &sets).unwrap();

        assert_eq!(
            groups,
            vec![Group {
                events: request(&["a", "b", "c"]),
                sampled: 0,
            }]
        );
    }

    #[test]
    fn a_formula_wider_than_a_stated_capacity_is_refused_not_split() {
        let family = family(Some(3), None, &[]);
        let counters = request(&["a", "b", "c"]);
        let sets = [customs(&["a", "b"]), customs(&["b", "c"])];

        // The NMI watchdog holds one of the three counters.
        assert_eq!(
            plan(Some(&family), 1, &counters, &sets),
            Err(TooWide {
                events: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
                available: 2,
            })
        );
    }

    #[test]
    fn a_wide_formula_gets_its_own_group_when_capacity_is_a_guess() {
        let counters = request(&["a", "b", "c", "d", "x"]);
        let groups = plan(None, 0, &counters, &[customs(&["a", "b", "c", "d"])]).unwrap();

        assert_eq!(
            groups
                .iter()
                .map(|group| &group.events[2..])
                .collect::<Vec<_>>(),
            vec![&customs(&["a", "b", "c", "d"])[..], &customs(&["x"])[..]]
        );
    }

    #[test]
    fn software_events_open_in_the_first_group_only() {
        let mut counters = request(&["a", "b", "c", "d"]);
        counters.extend([Counter::CpuClock, Counter::PageFaults]);
        let groups = plan(None, 0, &counters, &[]).unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(holder(&groups, &Counter::CpuClock), 0);
        assert_eq!(holder(&groups, &Counter::PageFaults), 0);
        for group in &groups {
            assert_eq!(group.events[..2], [Counter::Cycles, Counter::Instructions]);
            assert_eq!(group.sampled, 0);
        }
    }

    #[test]
    fn the_watchdog_counter_shrinks_every_group() {
        let family = family(Some(4), None, &[]);
        let counters = request(&["a", "b", "c", "d"]);

        assert_eq!(plan(Some(&family), 0, &counters, &[]).unwrap().len(), 1);
        assert_eq!(plan(Some(&family), 1, &counters, &[]).unwrap().len(), 2);
    }

    #[test]
    fn a_leader_that_cycles_resolves_to_is_not_opened_twice() {
        let family = family(Some(14), Some("cycles"), &[("cycles", "cycles")]);
        let counters = [Counter::Cycles, Counter::Instructions, custom("cycles")];
        let groups = plan(Some(&family), 0, &counters, &[]).unwrap();

        assert_eq!(
            groups,
            vec![Group {
                events: vec![Counter::Cycles, Counter::Instructions],
                sampled: 0,
            }]
        );
    }

    #[test]
    fn a_distinct_leader_event_leads_and_samples_every_group() {
        let family = family(None, Some("u_mode_cycle"), &[]);
        let counters = request(&["a", "b", "c"]);
        let groups = plan(Some(&family), 0, &counters, &[]).unwrap();

        // A leader leaves two events a group by default.
        assert_eq!(groups.len(), 2);
        for group in &groups {
            assert_eq!(
                group.events[..3],
                [
                    custom("u_mode_cycle"),
                    Counter::Cycles,
                    Counter::Instructions
                ]
            );
            assert_eq!(group.sampled, 0);
        }
    }

    #[test]
    fn perf_metrics_ride_free_behind_slots_and_cycles_samples() {
        let family = family(Some(1), None, &[]);
        let topdown = [
            "topdown_retiring",
            "slots",
            "topdown_bad_spec",
            "topdown_fe_bound",
            "topdown_be_bound",
        ];
        let mut counters = request(&topdown);
        counters.push(Counter::CpuClock);
        let groups = plan(Some(&family), 0, &counters, &[request(&topdown)]).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].events[0], custom("slots"));
        assert_eq!(groups[0].events[groups[0].sampled], Counter::Cycles);
        assert_eq!(groups[0].events.len(), counters.len());
    }

    #[test]
    fn without_cycles_cpu_clock_leads_a_single_group() {
        let counters = [Counter::PageFaults, Counter::CpuClock, Counter::LLCMisses];
        let groups = plan(None, 0, &counters, &[]).unwrap();

        assert_eq!(
            groups,
            vec![Group {
                events: vec![Counter::CpuClock, Counter::PageFaults, Counter::LLCMisses],
                sampled: 0,
            }]
        );
    }

    #[test]
    fn groups_share_the_host_sample_rate_ceiling() {
        assert_eq!(group_sample_freq(1000, 2, Some(1000)), 400);
        assert_eq!(group_sample_freq(1000, 1, Some(100_000)), 1000);
        assert_eq!(group_sample_freq(1000, 3, None), 1000);
    }

    /// Every shipped event table must yield groups the hardware can schedule
    /// and its own top-down formulas can be computed from, with and without
    /// the NMI watchdog holding a counter, and whatever order the table lists
    /// its events in.
    #[test]
    fn every_shipped_table_plans_whole_formulas_and_no_event_twice() {
        let snapshot = [
            Counter::Cycles,
            Counter::Instructions,
            Counter::LLCReferences,
            Counter::LLCMisses,
            Counter::BranchMisses,
            Counter::BranchInstructions,
            Counter::StalledCyclesBackend,
            Counter::StalledCyclesFrontend,
            Counter::CpuClock,
            Counter::PageFaults,
        ];

        for (id, family) in crate::cpu_family::families() {
            let tma = family.scenarios.get("tma");
            let sets: Vec<Vec<Counter>> = tma
                .iter()
                .flat_map(|scenario| &scenario.groups)
                .map(|group| group.events.iter().map(|e| crate::tma_counter(e)).collect())
                .collect();
            let mut requests = vec![snapshot.to_vec()];
            if let Some(scenario) = tma {
                let events: Vec<Counter> = scenario
                    .events
                    .iter()
                    .map(|event| crate::tma_counter(event))
                    .collect();
                requests.push(events.iter().rev().cloned().collect());
                requests.push(events);
            }

            for (counters, reserved) in requests.iter().flat_map(|r| [(r, 0), (r, 1)]) {
                let groups = plan(Some(family), reserved, counters, &sets)
                    .unwrap_or_else(|error| panic!("{id}: {error:?}"));

                for group in &groups {
                    let mut events: Vec<String> = group
                        .events
                        .iter()
                        .map(|counter| resolved_event(counter, Some(family)))
                        .collect();
                    let planned = events.len();
                    events.sort();
                    events.dedup();
                    assert_eq!(
                        planned,
                        events.len(),
                        "{id} opens an event twice: {group:?}"
                    );
                    assert!(group.events.contains(&Counter::Cycles), "{id}: no cycles");
                }

                for set in &sets {
                    let holders: HashSet<usize> = set
                        .iter()
                        .filter(|event| {
                            counters.contains(event)
                                && !matches!(event, Counter::Cycles | Counter::Instructions)
                        })
                        .map(|event| holder(&groups, event))
                        .collect();
                    assert!(
                        holders.len() <= 1,
                        "{id}: formula {set:?} is split across groups"
                    );
                }
            }
        }
    }
}
