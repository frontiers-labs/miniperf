# View results in the terminal

```sh
mperf show results/tma
```

`mperf show` opens a recording in a full-screen terminal viewer. It is the fastest way to look at a result on a remote machine.

## Keys

| Key | Action |
|---|---|
| `Tab`, `Shift-Tab` | Next and previous tab |
| `?` | Show or hide the help window |
| `q` | Quit |
| `Up`, `Down` | Move the selection in a table |
| `PageUp`, `PageDown` | Move five rows |
| `Home`, `End` | Jump to the first or last row |
| `Left`, `Right` | Scroll wide tables horizontally. The function column stays put. |
| `Enter` | Open the assembly view for the selected function. `Esc` or `Enter` closes it. |
| `m` | In the Flamegraph tab, switch between cycles and instructions |

## Tabs

The tabs depend on the scenario:

| Scenario | Tabs |
|---|---|
| `snapshot` | Summary, Resources, Hotspots, Flamegraph |
| `tma` | Summary, Hotspots, Flamegraph |
| `mem` | Summary, Memory, Flamegraph |
| `roofline` | Summary, Loops, Memory with the QEMU backend, Flamegraph |

**Summary** shows the recording-wide counters: cycles, instructions, IPC, branch and cache miss rates and MPKI, and stall percentages. Counters that were not recorded show `N/A`. Below them are the command, CPU, and for Roofline recordings the method, its warnings, and the calibrated ceilings.

**Hotspots** lists functions by share of cycles with cycles, instructions, and IPC. In a `tma` recording it adds one percent column per top-down metric. Press `Enter` on a row to see its disassembly with per-instruction sample counts. This needs `objdump`.

**Resources** is the snapshot findings table, titled `What to measure next`: severity, resource, finding, evidence, and the next measurement.

**Memory** shows the headline row of `memory_summary`: footprint, peak allocation, peak RSS, cold references, achieved and sustainable bandwidth, and utilization.

**Loops** lists Roofline loops with their location and, for scalar and vector single and double precision, GFLOP/s and arithmetic intensity.

**Flamegraph** renders the folded stacks as a text flame graph. Press `m` to switch weights between cycles and instructions.

Tables are sorted the way the scenario defines them. The terminal viewer has no interactive sorting or filtering. For that, use [`mperf query`](query.md) or the [GUI](gui.md).

## Old recordings

The viewer accepts recordings in format version 3. An older recording fails to open with a message asking you to record again.
