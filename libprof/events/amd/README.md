# AMD PMU event data

The event tables in this directory are generated from Linux perf's
`tools/perf/pmu-events/arch/x86/amdzen*` data. Regenerate one with:

```text
cargo run -p event-import -- amd-linux <linux>/tools/perf/pmu-events/arch/x86/amdzen5 \
  libprof/events/amd/zen5.json zen5 "AMD Zen 5"
```

| Table | Source directory | Name |
|---|---|---|
| `zen1.json` | `amdzen1` | `AMD Zen or Zen+ architecture` |
| `zen2.json` | `amdzen2` | `AMD Zen 2` |
| `zen3.json` | `amdzen3` | `AMD Zen 3` |
| `zen4.json` | `amdzen4` | `AMD Zen 4` |
| `zen5.json` | `amdzen5` | `AMD Zen 5` |
| `zen6.json` | `amdzen6` | `AMD Zen 6` |

Source revision: torvalds/linux `0c2669a9f4a1d607e7591ae50ccf3c432a0aff08`.
https://github.com/torvalds/linux/tree/master/tools/perf/pmu-events/arch/x86

Linux perf's event data is distributed under GPL-2.0-only. The source repository
and its `COPYING` file are authoritative for licensing and attribution.

The importer writes the whole file, including the top-down scenario, so do not
edit a table by hand. Change `utils/event-import` and regenerate. The model
ranges that select a table are in `libprof/src/cpu_family.rs` and follow perf's
`mapfile.csv`.
