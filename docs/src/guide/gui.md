# View results in the GUI

```sh
mperf-gui results/tma
mperf-gui
```

`mperf-gui` is the desktop viewer. It runs on Linux and macOS, and Windows packages ship it on its own for viewing recordings made elsewhere. Without a directory argument it opens a welcome screen with an **Open recording** button and a list of recent recordings.

## Layout

From top to bottom: a title bar, a filter bar, the master timeline, a tab strip, the active view with an optional selection panel on the right, and a status bar.

**Title bar.** The recordings dropdown shows the current recording and its scenario, lists recent recordings, and has an **Open recording** item. The theme toggle overrides the system light or dark setting.

**Filter bar.** One global filter scopes every view: a thread selector, a module selector, and a symbol text filter. Press `Ctrl-F` (`Cmd-F` on macOS) to jump to the symbol filter, and `Esc` to clear the staged selection.

**Master timeline.** Per-thread activity lanes and up to three pinned counter tracks. Drag anywhere on it to restrict every view to a time range. Click the header to collapse it.

**Selection panel.** Statistics for the selected function: IPC, LLC MPKI, and backend stall share when available, otherwise self and total share and sample count, with a link to its source.

**Status bar.** Recording name, scenario, CPU model, duration, sample count and frequency, and whether a filter is active.

## Views

A tab appears only when the recording has its data.

| View | Shows |
|---|---|
| Summary | Elapsed time, CPU time, IPC, the top-down bar, the top hotspot, findings, memory and Roofline headlines, and which counters were recorded |
| Hotspots | Sortable function table: self and total share, then CPU time, IPC, LLC MPKI, backend stall, and branch MPKI where recorded. Click a column header to sort. |
| Flame Graph | Icicle chart. Switch between top-down and bottom-up, and between cycles and instructions. Hover for details, click to select, double-click to zoom, **Reset zoom** to return. Colors follow module kind, and symbol-filter matches stay lit. |
| Flame Scope | The run folded into one column per second, with sub-second offset on the vertical axis and sample density as heat. Drag horizontally to set the time filter. |
| Timeline | Every thread lane and counter track, split into process-scoped and socket-wide groups |
| Cores | Per-CPU occupancy lanes, a concurrency histogram, and a per-thread balance table |
| Top-Down | The metric hierarchy, pipeline slots over time, and the per-function level-1 breakdown |
| Resources | The snapshot USE cards, one per resource with utilization, saturation, and error rows and sparklines, plus the ranked findings |
| Memory | Bandwidth and residency over time, the miss-ratio curve, stride and line-use histograms, and the working-set table |
| Roofline | Loops plotted against the calibrated ceilings, a detail panel for the selected loop, and the loop table. Scroll to zoom, drag to pan, click to select, double-click a point to open its source, double-click empty space to reset. |

**Source tabs.** Open a function from a hotspot row, a flame graph frame, or a Roofline point. The tab shows source on the left and disassembly on the right, both with sample heat in the gutter. Hovering a line highlights its counterpart. Clicking pins the link. Disassembly needs `objdump` and loads on demand.

## Persisted state

The GUI remembers the twelve most recent recordings in `recent-results.json`, under `~/.local/state/mperf-gui` on Linux, `~/Library/Application Support/mperf-gui` on macOS, or the directory named by `MPERF_GUI_STATE_DIR`. Nothing else persists between sessions.

## Build notes

On Linux the GUI needs the `xcb` and `xkbcommon` development libraries to build. On macOS it needs Xcode. If Cargo cannot find the SDK, run the build with `SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"`. See [Install miniperf](install.md).
