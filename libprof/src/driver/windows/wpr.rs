//! Model-specific WPR profiles for one coherent PMU counter group.
//!
//! WPR registers custom profile sources by event/umask, then records their
//! values on context switches. This does not make perf-only events available
//! to the Windows HAL. The returned XML still needs validation on the target
//! machine before a recording is started.

use std::collections::HashSet;
use std::fmt::Write;

use pmu_data::{EventDesc, TmaGroup, TmaScenario};

use crate::cpu_family::{find_cpu_family, CPUFamily};
use crate::{Counter, Error};

/// A WPR profile for the complete host top-down scenario. Each entry maps a
/// scenario event to the counter snapshot emitted at a context switch.
#[derive(Debug)]
pub struct WindowsTmaProfile {
    /// WPR profile XML to save as a `.wprp` file.
    pub xml: String,
    /// Profile name to pass to WPR when recording.
    pub profile_name: String,
    /// Scenario events in the PMC vector order emitted by WPR.
    pub counters: Vec<(String, Counter)>,
}

/// Build one coherent WPR counter profile for the detected CPU's TMA scenario.
///
/// A recording must still be validated and started by WPR on the target host.
pub fn windows_tma_profile() -> Result<WindowsTmaProfile, Error> {
    let family_id = crate::cpu_family::get_host_cpu_family();
    let family =
        find_cpu_family(family_id).ok_or_else(|| invalid("no PMU event table for the host CPU"))?;
    let scenario = family
        .scenarios
        .get("tma")
        .ok_or_else(|| invalid("no model-specific TMA scenario for the host CPU"))?;
    let cycle_source = super::wpr_tma_source_name(&Counter::Cycles)?;
    let instruction_source = super::wpr_tma_source_name(&Counter::Instructions)?;
    #[cfg(target_arch = "x86_64")]
    let (family_number, model, stepping, host_capacity) = host_cpuid()?;
    #[cfg(not(target_arch = "x86_64"))]
    return Err(invalid("WPR custom PMU sources require an x86-64 host"));
    #[cfg(target_arch = "x86_64")]
    generate_for_scenario(
        CpuIdentity {
            family_id,
            family: family_number,
            model,
            stepping: Some(stepping),
        },
        family,
        scenario,
        [&cycle_source, &instruction_source],
        host_capacity,
    )
}

/// Build a WPR profile containing exactly the requested programmable counters.
/// Model-specific scenario events are registered from the CPU event table;
/// ordinary Windows profile sources retain their HAL names.
pub fn windows_counter_profile(requested: &[Counter]) -> Result<WindowsTmaProfile, Error> {
    let family_id = crate::cpu_family::get_host_cpu_family();
    let family =
        find_cpu_family(family_id).ok_or_else(|| invalid("no PMU event table for the host CPU"))?;
    let scenario = family.scenarios.get("tma");
    let use_fixed_baseline = requested
        .iter()
        .any(|counter| matches!(counter, Counter::Custom(_)));
    let hal_names = requested
        .iter()
        .map(|counter| {
            let model_specific = scenario.is_some_and(|scenario| {
                scenario
                    .events
                    .iter()
                    .any(|event| event.eq_ignore_ascii_case(counter.name()))
            }) && !matches!(counter, Counter::Cycles | Counter::Instructions);
            if model_specific || counter.is_software() {
                Ok(None)
            } else if use_fixed_baseline
                && matches!(counter, Counter::Cycles | Counter::Instructions)
            {
                super::wpr_tma_source_name(counter).map(Some)
            } else {
                super::wpr_profile_source_name(counter).map(Some)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(target_arch = "x86_64")]
    let (family_number, model, stepping, host_capacity) = host_cpuid()?;
    #[cfg(not(target_arch = "x86_64"))]
    return Err(invalid("WPR custom PMU sources require an x86-64 host"));
    #[cfg(target_arch = "x86_64")]
    generate_for_requested(
        CpuIdentity {
            family_id,
            family: family_number,
            model,
            stepping: Some(stepping),
        },
        family,
        requested,
        &hal_names,
        host_capacity,
    )
}

fn generate_for_requested(
    cpu: CpuIdentity<'_>,
    family: &CPUFamily,
    requested: &[Counter],
    hal_names: &[Option<String>],
    host_capacity: usize,
) -> Result<WindowsTmaProfile, Error> {
    if requested.is_empty() {
        return Err(invalid("WPR profile has no programmable counters"));
    }
    if requested.len() != hal_names.len() {
        return Err(invalid(
            "WPR profile source names do not match requested counters",
        ));
    }
    let architecture = match family.vendor.as_str() {
        "Intel" => "INTEL",
        "AMD" => "AMD",
        _ => return Err(invalid("WPR custom x86 PMU sources require Intel or AMD")),
    };
    let scenario = family.scenarios.get("tma");
    let mut seen = HashSet::new();
    let mut sources = Vec::with_capacity(requested.len());
    let mut counters = Vec::with_capacity(requested.len());
    for (counter, hal_name) in requested.iter().zip(hal_names) {
        if !seen.insert(counter.name().to_ascii_lowercase()) {
            return Err(invalid("WPR profile contains a duplicate counter"));
        }
        if counter.is_software() {
            return Err(invalid(
                "software counters do not belong in a WPR hardware profile",
            ));
        }
        let tma_name = scenario.and_then(|scenario| {
            scenario
                .events
                .iter()
                .find(|name| name.eq_ignore_ascii_case(counter.name()))
        });
        let source = match hal_name {
            Some(name) => (name.clone(), None),
            None => {
                let name = tma_name.map(String::as_str).unwrap_or(counter.name());
                let origin = family.aliases.get(name).map(String::as_str).unwrap_or(name);
                let event = family.events.get(origin).ok_or_else(|| {
                    invalid(&format!(
                        "PMU event {name} is absent from the {} table",
                        family.id
                    ))
                })?;
                (String::new(), Some(encode_event(event, architecture)?))
            }
        };
        sources.push(source);
        counters.push((counter.name().to_owned(), counter.clone()));
    }
    let programmable = sources
        .iter()
        .filter(|(name, encoding)| encoding.is_some() || !is_fixed_source(name))
        .count();
    if family.max_counters.is_some_and(|max| programmable > max) || programmable > host_capacity {
        return Err(invalid(
            "requested events exceed the programmable counter capacity",
        ));
    }
    let (xml, profile_name, order) = render_profile(cpu, family, "requested events", &sources)?;
    let counters = order
        .into_iter()
        .map(|index| counters[index].clone())
        .collect();
    Ok(WindowsTmaProfile {
        xml,
        profile_name,
        counters,
    })
}

#[cfg(target_arch = "x86_64")]
fn host_cpuid() -> Result<(u32, u32, u32, usize), Error> {
    use core::arch::x86_64::__cpuid;
    let max_leaf = __cpuid(0).eax;
    let signature = __cpuid(1).eax;
    let base_family = (signature >> 8) & 0xf;
    let base_model = (signature >> 4) & 0xf;
    let family = base_family
        + if base_family == 0xf {
            (signature >> 20) & 0xff
        } else {
            0
        };
    let model = base_model
        + if base_family == 6 || base_family == 0xf {
            ((signature >> 16) & 0xf) << 4
        } else {
            0
        };
    let stepping = signature & 0xf;
    let vendor = __cpuid(0);
    let capacity = if (vendor.ebx, vendor.edx, vendor.ecx) == (0x756e6547, 0x49656e69, 0x6c65746e) {
        // Intel Architectural Performance Monitoring: EAX[15:8].
        if max_leaf < 0xa {
            return Err(invalid(
                "host CPUID does not report programmable PMU capacity",
            ));
        }
        ((__cpuid(0xa).eax >> 8) & 0xff) as usize
    } else if (vendor.ebx, vendor.edx, vendor.ecx) == (0x68747541, 0x69746e65, 0x444d4163) {
        let max_extended = __cpuid(0x8000_0000).eax;
        if max_extended >= 0x8000_0022 {
            let reported = (__cpuid(0x8000_0022).ebx & 0xf) as usize;
            if reported != 0 {
                reported
            } else {
                amd_legacy_capacity(max_extended)
            }
        } else {
            amd_legacy_capacity(max_extended)
        }
    } else {
        return Err(invalid("WPR custom PMU sources require Intel or AMD"));
    };
    if capacity == 0 {
        return Err(invalid("host CPUID reports no programmable PMU counters"));
    }
    Ok((family, model, stepping, capacity))
}

#[cfg(target_arch = "x86_64")]
fn amd_legacy_capacity(max_extended: u32) -> usize {
    // PerfCtrExtCore (CPUID 80000001h ECX bit 23) adds two counters to
    // the four architectural core counters on earlier AMD families.
    if max_extended >= 0x8000_0001 && (__cpuid_extended_features() & (1 << 23)) != 0 {
        6
    } else {
        4
    }
}

#[cfg(target_arch = "x86_64")]
fn __cpuid_extended_features() -> u32 {
    core::arch::x86_64::__cpuid(0x8000_0001).ecx
}

/// Processor identity used by WPR to match the custom counter definitions.
/// `family` and `model` are the decoded CPUID values, not the table ID.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CpuIdentity<'a> {
    pub family_id: &'a str,
    pub family: u32,
    pub model: u32,
    pub stepping: Option<u32>,
}

/// Build a unique WPRP for a named group in a shipped scenario.
///
/// Counter entries are ordered by source name to match WPR's PMC vector.
/// The profile records counter snapshots at CSwitch events; it does not sample
/// on counter overflow. WPR registration and counter availability must still
/// be checked on the machine that will record the trace.
pub(crate) fn generate_wprp(
    cpu: CpuIdentity<'_>,
    scenario_name: &str,
    group_name: &str,
) -> Result<String, Error> {
    let family =
        find_cpu_family(cpu.family_id).ok_or_else(|| invalid("unknown CPU event table"))?;
    let scenario = family
        .scenarios
        .get(scenario_name)
        .ok_or_else(|| invalid("unknown PMU scenario"))?;
    let group = scenario
        .groups
        .iter()
        .find(|group| group.name == group_name)
        .ok_or_else(|| invalid("unknown coherent PMU event group"))?;
    generate_for_group(cpu, family, group)
}

fn invalid(message: &str) -> Error {
    Error::InvalidConfiguration(message.to_owned())
}

fn is_fixed_source(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("fixed")
}

fn xml_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c == '\t' || c == '\n' || c == '\r' || c >= ' ' => out.push(c),
            _ => out.push('\u{fffd}'),
        }
    }
    out
}

struct Encoding {
    event: u16,
    unit: u8,
    extended_bits: Option<String>,
}

fn encode_event(event: &EventDesc, architecture: &str) -> Result<Encoding, Error> {
    let name = event.name.to_ascii_uppercase();
    let desc = event.desc.to_ascii_uppercase();
    // The dotted TOPDOWN.SLOTS_P and TOPDOWN.BACKEND_BOUND_SLOTS names
    // in the shipped Intel table are ordinary event-select A4 sources.
    // Linux's lowercase topdown_* / slots aliases refer to fixed
    // PERF_METRICS machinery and cannot be programmed through WPR here.
    if name.starts_with("TOPDOWN_") || name == "SLOTS" || name.contains("PERF_METRICS") {
        return Err(invalid(&format!(
            "{} uses fixed topdown/PERF_METRICS semantics unsupported by WPR custom sources",
            event.name
        )));
    }
    if name.contains("OFFCORE")
        || name.contains("LBR")
        || desc.contains(" MSR")
        || desc.contains("MSR ")
        || desc.contains("PEBS")
        || desc.contains("LOAD LATENCY")
    {
        return Err(invalid(&format!(
            "{} requires auxiliary MSR or precise-event configuration unavailable in WPR ProfileSource",
            event.name
        )));
    }

    let code = event.code;
    // perf raw config: event 0:7, umask 8:15, edge 18, any-thread 21,
    // invert 23, cmask 24:31. WPR exposes the latter four in ExtendedBits.
    const EXPRESSIBLE: u64 = 0xffff | (1 << 18) | (1 << 21) | (1 << 23) | (0xff << 24);
    if code == 0 || code & !EXPRESSIBLE != 0 {
        return Err(invalid(&format!(
            "{} has raw PMU bits that WPR ProfileSource cannot represent (0x{code:x})",
            event.name
        )));
    }
    let event_id = (code & 0xff) as u16;
    let unit = ((code >> 8) & 0xff) as u8;
    let cmask = ((code >> 24) & 0xff) as u8;
    let invert = ((code >> 23) & 1) as u8;
    let any_thread = ((code >> 21) & 1) as u8;
    let edge = ((code >> 18) & 1) as u8;
    if cmask > 99 {
        return Err(invalid(&format!(
            "{} has a counter mask too large for WPR's two-digit ExtendedBits field",
            event.name
        )));
    }
    if architecture == "AMD" && (cmask != 0 || invert != 0 || any_thread != 0 || edge != 0) {
        return Err(invalid(&format!(
            "{} uses extended raw bits not documented for AMD WPR ProfileSource",
            event.name
        )));
    }
    let extended_bits = (cmask != 0 || invert != 0 || any_thread != 0 || edge != 0)
        .then(|| format!("{cmask:02}{invert:02}{any_thread:02}{edge:02}"));
    Ok(Encoding {
        event: event_id,
        unit,
        extended_bits,
    })
}

fn generate_for_group(
    cpu: CpuIdentity<'_>,
    family: &CPUFamily,
    group: &TmaGroup,
) -> Result<String, Error> {
    let architecture = match family.vendor.as_str() {
        "Intel" => "INTEL",
        "AMD" => "AMD",
        _ => return Err(invalid("WPR custom x86 PMU sources require Intel or AMD")),
    };
    if group.events.is_empty() {
        return Err(invalid("PMU event group is empty"));
    }
    if family
        .max_counters
        .is_some_and(|max| group.events.len() > max)
    {
        return Err(invalid("PMU event group exceeds the table's counter limit"));
    }

    let mut seen = HashSet::new();
    let mut sources = Vec::with_capacity(group.events.len());
    for name in &group.events {
        if !seen.insert(name.as_str()) {
            return Err(invalid("PMU event group contains a duplicate event"));
        }
        let origin = family
            .aliases
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| {
                // The early Zen JSON has no portable aliases, but contains the
                // programmable architectural events used by its TMA groups.
                match (family.vendor.as_str(), name.as_str()) {
                    ("AMD", "cycles") => "ls_not_halted_cyc",
                    ("AMD", "instructions") => "ex_ret_instr",
                    _ => name.as_str(),
                }
            });
        let event = family.events.get(origin).ok_or_else(|| {
            invalid(&format!(
                "PMU event {name} is absent from the {} table",
                family.id
            ))
        })?;
        sources.push((String::new(), Some(encode_event(event, architecture)?)));
    }

    render_profile(cpu, family, &group.name, &sources).map(|(xml, _, _)| xml)
}

fn generate_for_scenario(
    cpu: CpuIdentity<'_>,
    family: &CPUFamily,
    scenario: &TmaScenario,
    hal_sources: [&str; 2],
    host_capacity: usize,
) -> Result<WindowsTmaProfile, Error> {
    let architecture = match family.vendor.as_str() {
        "Intel" => "INTEL",
        "AMD" => "AMD",
        _ => return Err(invalid("WPR custom x86 PMU sources require Intel or AMD")),
    };
    if scenario.events.is_empty() {
        return Err(invalid("TMA scenario has no events"));
    }
    let mut seen = HashSet::new();
    let mut sources = Vec::with_capacity(scenario.events.len());
    let mut counters = Vec::with_capacity(scenario.events.len());
    for name in &scenario.events {
        if !seen.insert(name.as_str()) {
            return Err(invalid("TMA scenario contains a duplicate event"));
        }
        let (source, encoding, counter) = match name.as_str() {
            "cycles" => (hal_sources[0].to_owned(), None, Counter::Cycles),
            "instructions" => (hal_sources[1].to_owned(), None, Counter::Instructions),
            _ => {
                let origin = family.aliases.get(name).map(String::as_str).unwrap_or(name);
                let event = family.events.get(origin).ok_or_else(|| {
                    invalid(&format!(
                        "TMA event {name} is absent from the {} table",
                        family.id
                    ))
                })?;
                (
                    String::new(),
                    Some(encode_event(event, architecture)?),
                    Counter::Custom(name.clone()),
                )
            }
        };
        sources.push((source, encoding));
        counters.push((name.clone(), counter));
    }
    let programmable = sources
        .iter()
        .filter(|(name, encoding)| encoding.is_some() || !is_fixed_source(name))
        .count();
    if family.max_counters.is_some_and(|max| programmable > max) {
        return Err(invalid(
            "TMA scenario exceeds the table's programmable counter limit",
        ));
    }
    if programmable > host_capacity {
        return Err(invalid(
            "TMA scenario exceeds the host's programmable counter capacity",
        ));
    }
    let (xml, profile_name, order) = render_profile(cpu, family, &scenario.name, &sources)?;
    let counters = order
        .into_iter()
        .map(|index| counters[index].clone())
        .collect();
    Ok(WindowsTmaProfile {
        xml,
        profile_name,
        counters,
    })
}

fn render_profile(
    cpu: CpuIdentity<'_>,
    family: &CPUFamily,
    description: &str,
    sources: &[(String, Option<Encoding>)],
) -> Result<(String, String, Vec<usize>), Error> {
    let architecture = match family.vendor.as_str() {
        "Intel" => "INTEL",
        "AMD" => "AMD",
        _ => return Err(invalid("WPR custom x86 PMU sources require Intel or AMD")),
    };
    // A UUID keeps WPR source names and all profile IDs separate from earlier
    // custom registrations, even when two profiles use the same event table.
    let unique = uuid::Uuid::now_v7().simple().to_string();
    let prefix = format!("miniperf_{unique}");
    let source_names: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(index, (source, _))| {
            if source.is_empty() {
                format!("{prefix}_counter_{index}")
            } else {
                source.clone()
            }
        })
        .collect();
    // WPR enumerates the PMC vector by profile-source name, even when the
    // HardwareCounter XML lists names in a different order. Keep the XML and
    // the decoder's event mapping in that same order. In particular, fixed
    // source names may sort on either side of custom sources.
    let mut order: Vec<usize> = (0..source_names.len()).collect();
    order.sort_by_key(|&index| source_names[index].to_ascii_lowercase());
    let mut xml = String::new();
    writeln!(xml, "<?xml version=\"1.0\" encoding=\"utf-8\"?>").unwrap();
    writeln!(
        xml,
        "<WindowsPerformanceRecorder Version=\"1.0\" Author=\"miniperf\">"
    )
    .unwrap();
    writeln!(xml, "  <Profiles>").unwrap();
    writeln!(
        xml,
        "    <SystemCollector Id=\"{prefix}_collector\" Name=\"{prefix}_collector\"/>"
    )
    .unwrap();
    // Timer profile samples carry instruction pointers for statistical
    // attribution of the coherent CSwitch counter intervals to functions.
    writeln!(xml, "    <SystemProvider Id=\"{prefix}_system\"><Keywords><Keyword Value=\"ProcessThread\"/><Keyword Value=\"Loader\"/><Keyword Value=\"CSwitch\"/><Keyword Value=\"SampledProfile\"/></Keywords></SystemProvider>").unwrap();
    let custom_sources = sources.iter().any(|(_, encoding)| encoding.is_some());
    if custom_sources {
        writeln!(
            xml,
            "    <MicroArchitecturalConfig Id=\"{prefix}_config\" Base=\"\" Strict=\"true\">"
        )
        .unwrap();
        write!(
            xml,
            "      <ProfileSources Architecture=\"{architecture}\" Family=\"{}\" Model=\"{}\"",
            cpu.family, cpu.model
        )
        .unwrap();
        if let Some(stepping) = cpu.stepping {
            write!(xml, " Stepping=\"{stepping}\"").unwrap();
        }
        writeln!(xml, " Description=\"{}\">", xml_attr(&family.name)).unwrap();
        for ((_, encoding), source_name) in sources.iter().zip(&source_names) {
            let Some(encoding) = encoding else {
                continue;
            };
            write!(xml, "        <ProfileSource Name=\"{source_name}\" Event=\"0x{:x}\" Unit=\"0x{:x}\" Persist=\"false\"", encoding.event, encoding.unit).unwrap();
            if let Some(bits) = &encoding.extended_bits {
                write!(xml, " ExtendedBits=\"{bits}\"").unwrap();
            }
            writeln!(xml, "/>").unwrap();
        }
        writeln!(xml, "      </ProfileSources>").unwrap();
        writeln!(xml, "    </MicroArchitecturalConfig>").unwrap();
    }
    writeln!(
        xml,
        "    <HardwareCounter Id=\"{prefix}_hardware\" Base=\"\" Strict=\"true\">"
    )
    .unwrap();
    if custom_sources {
        writeln!(
            xml,
            "      <MicroArchitecturalConfigId Value=\"{prefix}_config\"/>"
        )
        .unwrap();
    }
    writeln!(xml, "      <Counters>").unwrap();
    for &index in &order {
        writeln!(
            xml,
            "        <Counter Value=\"{}\"/>",
            xml_attr(&source_names[index])
        )
        .unwrap();
    }
    writeln!(
        xml,
        "      </Counters><Events><Event Value=\"CSwitch\"/></Events>"
    )
    .unwrap();
    writeln!(xml, "    </HardwareCounter>").unwrap();
    writeln!(xml, "    <Profile Id=\"{prefix}.Verbose.File\" Name=\"{prefix}\" Description=\"{}\" LoggingMode=\"File\" DetailLevel=\"Verbose\">", xml_attr(&format!("{}: {}", family.name, description))).unwrap();
    writeln!(xml, "      <Collectors><SystemCollectorId Value=\"{prefix}_collector\"><SystemProviderId Value=\"{prefix}_system\"/><HardwareCounterId Value=\"{prefix}_hardware\"/></SystemCollectorId></Collectors>").unwrap();
    writeln!(xml, "    </Profile>").unwrap();
    writeln!(xml, "  </Profiles>").unwrap();
    writeln!(xml, "</WindowsPerformanceRecorder>").unwrap();
    Ok((xml, prefix, order))
}
