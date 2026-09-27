//! Windows-specific policy for `mperf stat`.

#[cfg(target_os = "windows")]
mod imp {
    use anyhow::Result;
    use libprof::{Counter, CounterResult};

    use super::super::{
        find_counter, pmu_counters, push_counter, render_table, render_topdown,
        requested_counters_and_metrics, software_counters,
    };

    pub(crate) fn try_do_stat(
        pid: Option<u32>,
        command: &[String],
        event_names: &[String],
        topdown_level: Option<u8>,
    ) -> Result<bool> {
        if topdown_level.is_some() && !event_names.is_empty() {
            anyhow::bail!(
                "Windows `mperf stat --topdown` does not accept explicit events; run separate `mperf stat` commands"
            );
        }
        if let Some(level) = topdown_level {
            let (scenario, result) = crate::windows_tma::stat_topdown(pid, command)?;
            eprintln!(
                "notice: Windows Top-down counts cover observed CSwitch intervals; initial and final running intervals are not included"
            );
            render_topdown(&scenario, level, &result);
            return Ok(true);
        }
        if !event_names.is_empty() {
            let supported = libprof::list_supported_counters(libprof::DriverKind::Default);
            let scenario = libprof::tma_scenario();
            let mut available = supported.clone();
            add_explicit_candidates(&mut available);
            if let Some(scenario) = scenario.as_ref() {
                for name in &scenario.events {
                    push_counter(&mut available, libprof::tma_counter(name));
                }
            }
            let (counters, metrics) =
                requested_counters_and_metrics(event_names, &available, &libprof::host_metrics())?;
            let selected_names = counters
                .iter()
                .map(|counter| counter.name().to_owned())
                .collect::<Vec<_>>();
            if windows_explicit_event_route(&selected_names, &supported, scenario.as_ref())
                == WindowsExplicitEventRoute::Tma
            {
                if scenario.is_some() {
                    let result = crate::windows_tma::stat_explicit(pid, command, &counters)?;
                    require_counter_results(&counters, &result)?;
                    eprintln!(
                        "notice: Windows model-specific counters cover observed CSwitch intervals; initial and final running intervals are not included"
                    );
                    println!(
                        "\nPerformance counter stats for '{}':\n",
                        pid.map_or_else(|| command.join(" "), |pid| format!("pid {pid}"))
                    );
                    println!(
                        "{}",
                        render_table(&counters, &metrics, |counter| result.get(counter.clone()))
                    );
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn notice_counter_availability(supported: &[Counter], _hardware_counters: bool) {
        if accounting_only(supported) {
            eprintln!(
                "notice: ETW process PMU counting is unavailable; cycles, CPU time, and page faults use Windows process accounting"
            );
        }
    }

    pub(crate) fn filter_default_counters(defaults: &mut Vec<Counter>, supported: &[Counter]) {
        defaults.retain(|counter| supported.contains(counter));
        if let Some(limit) = crate::windows_tma::max_pmc_sources() {
            let mut selected = 0usize;
            let mut skipped = Vec::new();
            defaults.retain(|counter| {
                if counter.is_software() {
                    return true;
                }
                selected += 1;
                if selected > limit {
                    skipped.push(counter.name().to_owned());
                    false
                } else {
                    true
                }
            });
            if !skipped.is_empty() {
                eprintln!(
                    "notice: ETW can run at most {limit} profile sources together; omitting default events: {}",
                    skipped.join(", ")
                );
            }
        }
    }

    pub(crate) fn add_explicit_candidates(available: &mut Vec<Counter>) {
        // An explicit request should reach the Windows driver and report the
        // real ETW/PMU access error, even when a capability probe was denied.
        for counter in pmu_counters().into_iter().chain(software_counters()) {
            push_counter(available, counter);
        }
    }

    pub(crate) fn strict_explicit_events(explicit_events: bool) -> bool {
        explicit_events
    }

    pub(crate) fn require_explicit_results(
        explicit_events: bool,
        counters: &[Counter],
        result: &CounterResult,
    ) -> Result<()> {
        if explicit_events {
            require_counter_results(counters, result)?;
        }
        Ok(())
    }

    pub(crate) fn notice_cycles(counters: &[Counter], supported: &[Counter]) {
        if accounting_only(supported) && counters.contains(&Counter::Cycles) {
            eprintln!(
                "notice: cycles use Windows process cycle accounting when ETW PMU counting is unavailable; the accounting value includes kernel execution"
            );
        }
    }

    fn accounting_only(supported: &[Counter]) -> bool {
        supported.iter().all(|counter| {
            matches!(
                counter,
                Counter::Cycles | Counter::CpuClock | Counter::PageFaults
            )
        })
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum WindowsExplicitEventRoute {
        Driver,
        Tma,
    }

    /// Model-specific TMA events need WPR registration. The selected WPR capture
    /// can also include ordinary PMU sources and process accounting counters.
    fn windows_explicit_event_route(
        names: &[String],
        supported: &[Counter],
        scenario: Option<&pmu_data::TmaScenario>,
    ) -> WindowsExplicitEventRoute {
        let Some(scenario) = scenario else {
            return WindowsExplicitEventRoute::Driver;
        };
        let needs_registration = names.iter().any(|name| {
            scenario
                .events
                .iter()
                .any(|event| event.eq_ignore_ascii_case(name))
                && find_counter(supported, name).is_none()
        });
        if needs_registration {
            WindowsExplicitEventRoute::Tma
        } else {
            WindowsExplicitEventRoute::Driver
        }
    }

    fn require_counter_results(counters: &[Counter], result: &CounterResult) -> Result<()> {
        let missing = counters
            .iter()
            .filter(|counter| result.get((*counter).clone()).is_none())
            .map(Counter::name)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            anyhow::bail!(
                "Windows did not return the requested counter(s): {}",
                missing.join(", ")
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use libprof::Metric;

        #[test]
        fn mixed_model_event_and_accounting_uses_one_wpr_capture() {
            let scenario = pmu_data::TmaScenario {
                name: "tma".into(),
                events: vec!["cycles".into(), "model_event".into()],
                groups: Vec::new(),
                precise_attribution: false,
                constants: Vec::new(),
                metrics: Vec::new(),
                ui: None,
            };
            let supported = [Counter::Cycles, Counter::CpuClock, Counter::ContextSwitches];
            let names = vec![
                "model_event".into(),
                "cpu_clock".into(),
                "context_switches".into(),
            ];
            assert_eq!(
                windows_explicit_event_route(&names, &supported, Some(&scenario)),
                WindowsExplicitEventRoute::Tma
            );
            let mut available = supported.to_vec();
            for name in &scenario.events {
                push_counter(&mut available, libprof::tma_counter(name));
            }
            let (selected, _) = requested_counters_and_metrics(&names, &available, &[]).unwrap();
            assert_eq!(
                selected,
                vec![
                    Counter::Custom("model_event".into()),
                    Counter::CpuClock,
                    Counter::ContextSwitches
                ]
            );
            let metric = Metric {
                name: "model_rate".into(),
                desc: String::new(),
                expression: pmu_data::MetricExpression("model_event / cycles".into()),
                unit: None,
            };
            let (expanded, _) = requested_counters_and_metrics(
                &["model_rate".into(), "cpu_clock".into()],
                &available,
                &[metric],
            )
            .unwrap();
            let expanded_names = expanded
                .iter()
                .map(|counter| counter.name().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                windows_explicit_event_route(&expanded_names, &supported, Some(&scenario)),
                WindowsExplicitEventRoute::Tma
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use anyhow::Result;
    use libprof::{Counter, CounterResult};

    pub(crate) fn try_do_stat(
        _: Option<u32>,
        _: &[String],
        _: &[String],
        _: Option<u8>,
    ) -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn notice_counter_availability(_: &[Counter], hardware_counters: bool) {
        if !hardware_counters {
            eprintln!(
                "notice: no hardware PMU detected (VM/container or permissions); hardware counters may be unavailable"
            );
        }
    }

    pub(crate) fn filter_default_counters(_: &mut Vec<Counter>, _: &[Counter]) {}
    pub(crate) fn add_explicit_candidates(_: &mut Vec<Counter>) {}
    pub(crate) fn strict_explicit_events(_: bool) -> bool {
        false
    }
    pub(crate) fn require_explicit_results(
        _: bool,
        _: &[Counter],
        _: &CounterResult,
    ) -> Result<()> {
        Ok(())
    }
    pub(crate) fn notice_cycles(_: &[Counter], _: &[Counter]) {}
}

pub(super) use imp::{
    add_explicit_candidates, filter_default_counters, notice_counter_availability, notice_cycles,
    require_explicit_results, strict_explicit_events, try_do_stat,
};
