use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use perf_event_open_sys::bindings::perf_event_attr;

const ROOT: &str = "/sys/bus/event_source/devices";

/// Core PMU sysfs directories, best first (`cpu`, then hybrid
/// `cpu_core`/`cpu_atom`, then each Arm pmuv3 instance).
pub fn core_pmu_paths() -> Vec<PathBuf> {
    let fixed = ["cpu", "cpu_core", "cpu_atom"]
        .into_iter()
        .map(|name| Path::new(ROOT).join(name));
    let pmuv3 = std::fs::read_dir(ROOT)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("armv8_pmuv3"))
        });
    let mut paths = fixed
        .chain(pmuv3)
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

/// Build a `perf_event_attr` from the first core PMU exposing `event`.
pub fn core_event_attr(event: &str) -> Option<perf_event_attr> {
    core_pmu_paths()
        .into_iter()
        .find_map(|pmu| event_attr(&pmu, event))
}

/// Build a `perf_event_attr` from a sysfs event alias (`events/<name>`, e.g.
/// `event=0xcd,umask=0x1,ldlat=3`) and the PMU's `format/` bitfield map. This
/// keeps the encoding tied to what the running kernel advertises rather than
/// to a hardcoded model table.
pub fn event_attr(pmu: &Path, event: &str) -> Option<perf_event_attr> {
    let alias = std::fs::read_to_string(pmu.join("events").join(event)).ok()?;
    let type_id = std::fs::read_to_string(pmu.join("type"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;

    let mut attr = perf_event_attr::default();
    attr.type_ = type_id;
    for term in alias.trim().split(',').filter(|term| !term.is_empty()) {
        let (name, value) = term.split_once('=').unwrap_or((term, "1"));
        let value = parse_number(value.trim())?;
        let (register, low, high) = parse_format(pmu, name.trim())?;
        let field = value << low;
        let mask = field_mask(low, high);
        let slot = match register {
            0 => &mut attr.config,
            1 => &mut attr.config1,
            2 => &mut attr.config2,
            _ => return None,
        };
        *slot = (*slot & !mask) | (field & mask);
    }

    Some(attr)
}

/// Whether an event alias configures a named `format/` term.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub fn alias_has_term(pmu: &Path, event: &str, term: &str) -> bool {
    std::fs::read_to_string(pmu.join("events").join(event))
        .is_ok_and(|alias| alias.split(',').any(|part| part.trim().starts_with(term)))
}

/// Overwrite one `format/` field of an already encoded event.
pub fn set_format_field(attr: &mut perf_event_attr, pmu: &Path, name: &str, value: u64) {
    if let Some((0, low, high)) = parse_format(pmu, name) {
        let mask = field_mask(low, high);
        attr.config = (attr.config & !mask) | ((value << low) & mask);
    }
}

fn field_mask(low: u32, high: u32) -> u64 {
    if high >= 63 {
        u64::MAX << low
    } else {
        ((1_u64 << (high - low + 1)) - 1) << low
    }
}

/// Parse `format/<name>`, e.g. `config:0-7` or `config1:16`, into
/// `(register index, low bit, high bit)`.
fn parse_format(pmu: &Path, name: &str) -> Option<(u8, u32, u32)> {
    let spec = std::fs::read_to_string(pmu.join("format").join(name)).ok()?;
    parse_format_spec(spec.trim())
}

fn parse_format_spec(spec: &str) -> Option<(u8, u32, u32)> {
    let (register, bits) = spec.split_once(':')?;
    let register = match register {
        "config" => 0,
        "config1" => 1,
        "config2" => 2,
        _ => return None,
    };
    let positions = bits
        .split(',')
        .filter_map(|part| match part.split_once('-') {
            Some((low, high)) => Some(low.parse::<u32>().ok()?..=high.parse::<u32>().ok()?),
            None => part.parse::<u32>().ok().map(|bit| bit..=bit),
        })
        .flatten()
        .collect::<BTreeSet<_>>();
    Some((register, *positions.first()?, *positions.last()?))
}

fn parse_number(value: &str) -> Option<u64> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}
