use crate::{cpu_family, Counter};

pub fn list_supported_counters() -> Vec<Counter> {
    let mut counters = vec![
        Counter::Cycles,
        Counter::Instructions,
        Counter::BranchInstructions,
        Counter::BranchMisses,
        Counter::LLCMisses,
        Counter::LLCReferences,
        Counter::CpuClock,
        Counter::PageFaults,
        Counter::CpuMigrations,
        Counter::ContextSwitches,
    ];

    let cpu_family = cpu_family::get_host_cpu_family();
    let events = cpu_family::find_cpu_family(cpu_family);

    if let Some(events) = events {
        for evt in events.events.values() {
            counters.push(Counter::Internal {
                name: evt.name.clone(),
                desc: evt.desc.clone(),
                code: evt.code,
            });
        }
    }

    counters
}

fn resolve_custom_for_family(name: &str, family_id: &str) -> Option<Counter> {
    let family = cpu_family::find_cpu_family(family_id)?;
    let event = family.events.get(name).or_else(|| {
        family
            .events
            .values()
            .find(|event| event.name.eq_ignore_ascii_case(name))
    })?;
    Some(Counter::Internal {
        name: event.name.clone(),
        desc: event.desc.clone(),
        code: event.code,
    })
}

/// Resolve a logical counter into the concrete event for a *specific* CPU
/// family, without assuming it is the host family. Used to open the same
/// logical counter on every cluster's PMU.
///
/// Returns `None` when the counter is a named event that the given family does
/// not implement (e.g. an A720-only microarchitectural event has no meaning on
/// the A520 cluster), so callers simply skip it there instead of counting a
/// differently-numbered event.
pub fn resolve_counter_for_family(
    counter: &Counter,
    family_id: &str,
    prefer_raw_counters: bool,
) -> Option<Counter> {
    match counter {
        Counter::Custom(name) => resolve_custom_for_family(name, family_id),

        // Generic hardware counters: remap to this family's architectural event
        // via the alias table when possible, otherwise keep the generic form.
        _ if prefer_raw_counters => {
            let alias_name = match counter {
                Counter::Cycles => "cycles",
                Counter::Instructions => "instructions",
                Counter::LLCMisses => "cache_misses",
                Counter::LLCReferences => "cache_references",
                Counter::BranchMisses => "branch_misses",
                Counter::BranchInstructions => "branches",
                Counter::StalledCyclesBackend => "stalled_cycles_backend",
                Counter::StalledCyclesFrontend => "stalled_cycles_frontend",
                // Software counters are not PMU-specific, and an already
                // concrete raw event came from this family's own table.
                _ => return Some(counter.clone()),
            };

            let event = cpu_family::find_cpu_family(family_id)
                .and_then(|info| info.events.get(info.aliases.get(alias_name)?));
            Some(match event {
                Some(evt) => Counter::Internal {
                    name: evt.name.clone(),
                    desc: evt.desc.clone(),
                    code: evt.code,
                },
                None => counter.clone(),
            })
        }

        _ => Some(counter.clone()),
    }
}

/// Resolve a logical counter for the host CPU family.
pub fn process_counter(
    counter: &Counter,
    prefer_raw_counters: bool,
) -> Result<Counter, crate::Error> {
    let family = cpu_family::get_host_cpu_family();
    resolve_counter_for_family(counter, family, prefer_raw_counters).ok_or_else(|| {
        crate::Error::UnsupportedCounter {
            counter: counter.name().to_owned(),
            family: family.to_owned(),
        }
    })
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn resolves_tiger_lake_custom_event_case_insensitively() {
        let counter = resolve_custom_for_family("l1d.replacement", pmu_data::INTEL_TIGERLAKE)
            .expect("Tiger Lake event must resolve");
        assert!(matches!(
            counter,
            Counter::Internal { ref name, code: 0x151, .. } if name == "L1D.REPLACEMENT"
        ));
    }
}
