//! Named trace regions and their invocation hierarchy, independent of sampled stacks.
use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::{
    profile::TimeRange,
    sql::{Connection, table_columns},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RegionNodeId(pub usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RegionSpanId(pub usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RegionSelection {
    Node(RegionNodeId),
    Invocation(RegionSpanId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpanBoundary {
    Complete,
    MissingBegin,
    MissingEnd,
    CrossThread,
}

#[derive(Debug)]
pub(crate) struct RegionSpan {
    pub node: RegionNodeId,
    pub process_id: u32,
    pub thread_id: u32,
    pub instance: u64,
    pub range: TimeRange,
    pub depth: usize,
    pub boundary: SpanBoundary,
    pub self_ranges: Vec<TimeRange>,
}

#[derive(Debug)]
pub(crate) struct RegionNode {
    pub name: String,
    pub context: String,
    pub parent: Option<RegionNodeId>,
    pub children: Vec<RegionNodeId>,
    pub spans: Vec<RegionSpanId>,
    pub count: u64,
    pub incomplete: u64,
    pub inclusive_ns: u64,
    pub self_ns: u64,
}

/// Merged, disjoint half-open ranges for each originating process and thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct RegionIntervals {
    pub by_thread: BTreeMap<(u32, u32), Vec<TimeRange>>,
}

impl RegionIntervals {
    pub fn contains(&self, process_id: u32, thread_id: u32, timestamp: u64) -> bool {
        let Some(ranges) = self.by_thread.get(&(process_id, thread_id)) else {
            return false;
        };
        let end = ranges.partition_point(|range| range.start_ns <= timestamp);
        end > 0 && timestamp < ranges[end - 1].end_ns
    }

    pub fn intersections(
        &self,
        process_id: u32,
        thread_id: u32,
        range: TimeRange,
    ) -> Vec<TimeRange> {
        let Some(ranges) = self.by_thread.get(&(process_id, thread_id)) else {
            return Vec::new();
        };
        let first = ranges.partition_point(|item| item.end_ns <= range.start_ns);
        ranges[first..]
            .iter()
            .take_while(|item| item.start_ns < range.end_ns)
            .map(|item| TimeRange {
                start_ns: item.start_ns.max(range.start_ns),
                end_ns: item.end_ns.min(range.end_ns),
            })
            .collect()
    }

    fn merge(&mut self) {
        for ranges in self.by_thread.values_mut() {
            ranges.sort_unstable_by_key(|range| (range.start_ns, range.end_ns));
            let mut merged: Vec<TimeRange> = Vec::new();
            for range in ranges.drain(..) {
                if let Some(last) = merged.last_mut()
                    && range.start_ns <= last.end_ns
                {
                    last.end_ns = last.end_ns.max(range.end_ns);
                } else if range.start_ns < range.end_ns {
                    merged.push(range);
                }
            }
            *ranges = merged;
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RegionData {
    pub nodes: Vec<RegionNode>,
    pub roots: Vec<RegionNodeId>,
    pub spans: Vec<RegionSpan>,
    pub range: Option<TimeRange>,
    pub error: Option<String>,
}

impl RegionData {
    pub fn load(connection: &Connection, profile_range: Option<TimeRange>) -> Self {
        match load_regions(connection, profile_range) {
            Ok(data) => data,
            Err(error) => Self {
                error: Some(format!("{error:#}")),
                ..Self::default()
            },
        }
    }

    pub fn intervals(&self, selection: RegionSelection) -> RegionIntervals {
        let mut intervals = RegionIntervals::default();
        let spans: Vec<_> = match selection {
            RegionSelection::Node(id) => self.nodes[id.0].spans.clone(),
            RegionSelection::Invocation(id) => vec![id],
        };
        for id in spans {
            let span = &self.spans[id.0];
            if span.boundary != SpanBoundary::Complete {
                continue;
            }
            intervals
                .by_thread
                .entry((span.process_id, span.thread_id))
                .or_default()
                .push(span.range);
        }
        intervals.merge();
        intervals
    }

    pub fn label(&self, selection: RegionSelection) -> String {
        let (node, start) = match selection {
            RegionSelection::Node(id) => (id, None),
            RegionSelection::Invocation(id) => {
                let span = &self.spans[id.0];
                (span.node, Some(span.range.start_ns))
            }
        };
        let mut path = Vec::new();
        let mut current = Some(node);
        while let Some(id) = current {
            let node = &self.nodes[id.0];
            path.push(node.name.as_str());
            current = node.parent;
        }
        path.reverse();
        let label = path.join(" / ");
        if let Some(start) = start {
            let origin = self
                .range
                .expect("loaded regions have a recording range")
                .start_ns;
            format!(
                "{label} · at {:.1} ms",
                start.saturating_sub(origin) as f64 / 1e6
            )
        } else {
            label
        }
    }
}

struct RawSpan {
    event: u64,
    instance: u64,
    parent: u64,
    process_id: u32,
    thread_id: u32,
    range: TimeRange,
    boundary: SpanBoundary,
}

fn load_regions(connection: &Connection, profile_range: Option<TimeRange>) -> Result<RegionData> {
    if table_columns(connection, "events").is_empty() {
        return Ok(RegionData::default());
    }
    let mut statement = connection.prepare(
        "SELECT timestamp, event_id, instance, parent_id, flow_id, \"type\", pid, tid FROM events ORDER BY timestamp, \"type\", pid, tid, instance"
    ).context("failed to query trace regions")?;
    let events = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u8>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, u32>(7)?,
            ))
        })?
        .collect::<crate::sql::SqlResult<Vec<_>>>()?;
    let Some(first) = events.first() else {
        return Ok(RegionData::default());
    };
    let start = profile_range.map_or(first.0, |range| range.start_ns.min(first.0));
    let end = profile_range.map_or(events.last().unwrap().0.saturating_add(1), |range| {
        range.end_ns.max(events.last().unwrap().0.saturating_add(1))
    });
    let mut names = BTreeMap::new();
    if !table_columns(connection, "payloads").is_empty()
        && !table_columns(connection, "strings").is_empty()
    {
        let mut statement = connection.prepare(
            "SELECT DISTINCT p.event_id, COALESCE(n.string, ''), COALESCE(f.string, ''), COALESCE(s.string, ''), p.line FROM payloads p LEFT JOIN strings n ON n.id=p.name_id LEFT JOIN strings f ON f.id=p.function_id LEFT JOIN strings s ON s.id=p.file_id"
        )?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })? {
            let (id, name, function, file, line) = row?;
            names.insert(id, (name, format!("{function} · {file}:{line}")));
        }
    }
    let mut raw = Vec::new();
    let mut open = BTreeMap::new();
    for (timestamp, event, instance, parent, flow, kind, pid, tid) in events {
        match kind {
            0 => {
                let index = raw.len();
                raw.push(RawSpan {
                    event,
                    instance,
                    parent,
                    process_id: pid,
                    thread_id: tid,
                    range: TimeRange {
                        start_ns: timestamp,
                        end_ns: end,
                    },
                    boundary: SpanBoundary::MissingEnd,
                });
                open.insert((pid, instance), index);
            }
            1 => {
                if let Some(index) = open.remove(&(pid, flow)) {
                    let span = &mut raw[index];
                    span.range.end_ns = timestamp.max(span.range.start_ns);
                    span.boundary = if span.thread_id == tid {
                        SpanBoundary::Complete
                    } else {
                        SpanBoundary::CrossThread
                    };
                } else {
                    raw.push(RawSpan {
                        event,
                        instance: flow,
                        parent: 0,
                        process_id: pid,
                        thread_id: tid,
                        range: TimeRange {
                            start_ns: start,
                            end_ns: timestamp,
                        },
                        boundary: SpanBoundary::MissingBegin,
                    });
                }
            }
            _ => {}
        }
    }
    raw.sort_by_key(|span| {
        (
            span.range.start_ns,
            std::cmp::Reverse(span.range.end_ns),
            span.process_id,
            span.thread_id,
            span.instance,
        )
    });
    let identities: BTreeMap<_, _> = raw
        .iter()
        .enumerate()
        .map(|(i, span)| ((span.process_id, span.instance), i))
        .collect();
    let mut parents = vec![None; raw.len()];
    let mut active = BTreeMap::<(u32, u32), Vec<usize>>::new();
    let mut depths = Vec::new();
    for (index, span) in raw.iter().enumerate() {
        let stack = active.entry((span.process_id, span.thread_id)).or_default();
        stack.retain(|&parent| raw[parent].range.end_ns > span.range.start_ns);
        let container = stack.iter().rev().copied().find(|&parent| {
            raw[parent].range.end_ns >= span.range.end_ns
                && matches!(
                    raw[parent].boundary,
                    SpanBoundary::Complete | SpanBoundary::MissingEnd
                )
        });
        let mut depth = container.map_or(0, |parent| depths[parent] + 1);
        while stack.iter().any(|&other| depths[other] == depth) {
            depth += 1;
        }
        depths.push(depth);
        parents[index] = identities
            .get(&(span.process_id, span.parent))
            .copied()
            .filter(|&parent| {
                span.parent != 0
                    && parent < index
                    && (raw[parent].thread_id != span.thread_id
                        || raw[parent].range.start_ns <= span.range.start_ns
                            && raw[parent].range.end_ns >= span.range.end_ns)
            })
            .or(container);
        stack.push(index);
    }
    let mut child_ranges = vec![RegionIntervals::default(); raw.len()];
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            let child = &raw[index];
            let ancestor = &raw[*parent];
            if child.thread_id == ancestor.thread_id && child.boundary == SpanBoundary::Complete {
                child_ranges[*parent]
                    .by_thread
                    .entry((child.process_id, child.thread_id))
                    .or_default()
                    .push(TimeRange {
                        start_ns: child.range.start_ns.max(ancestor.range.start_ns),
                        end_ns: child.range.end_ns.min(ancestor.range.end_ns),
                    });
            }
        }
    }
    for children in &mut child_ranges {
        children.merge();
    }
    let mut data = RegionData {
        range: Some(TimeRange {
            start_ns: start,
            end_ns: end,
        }),
        ..RegionData::default()
    };
    let mut node_ids = BTreeMap::new();
    for (index, span) in raw.iter().enumerate() {
        let parent = parents[index].map(|parent| data.spans[parent].node);
        let node = *node_ids.entry((parent, span.event)).or_insert_with(|| {
            let id = RegionNodeId(data.nodes.len());
            let (name, context) = names
                .get(&span.event)
                .cloned()
                .unwrap_or_else(|| (format!("event {}", span.event), String::new()));
            data.nodes.push(RegionNode {
                name,
                context,
                parent,
                children: Vec::new(),
                spans: Vec::new(),
                count: 0,
                incomplete: 0,
                inclusive_ns: 0,
                self_ns: 0,
            });
            if let Some(parent) = parent {
                data.nodes[parent.0].children.push(id);
            } else {
                data.roots.push(id);
            }
            id
        });
        let node_data = &mut data.nodes[node.0];
        node_data.spans.push(RegionSpanId(index));
        if span.boundary == SpanBoundary::Complete {
            node_data.count += 1;
            node_data.inclusive_ns += span.range.end_ns - span.range.start_ns;
            let child_ns: u64 = child_ranges[index]
                .by_thread
                .values()
                .flatten()
                .map(|range| range.end_ns - range.start_ns)
                .sum();
            node_data.self_ns += (span.range.end_ns - span.range.start_ns).saturating_sub(child_ns);
        } else {
            node_data.incomplete += 1;
        }
        let mut self_ranges = Vec::new();
        let mut cursor = span.range.start_ns;
        for child in child_ranges[index].by_thread.values().flatten() {
            if cursor < child.start_ns {
                self_ranges.push(TimeRange {
                    start_ns: cursor,
                    end_ns: child.start_ns,
                });
            }
            cursor = cursor.max(child.end_ns);
        }
        if cursor < span.range.end_ns {
            self_ranges.push(TimeRange {
                start_ns: cursor,
                end_ns: span.range.end_ns,
            });
        }
        data.spans.push(RegionSpan {
            node,
            process_id: span.process_id,
            thread_id: span.thread_id,
            instance: span.instance,
            range: span.range,
            depth: depths[index],
            boundary: span.boundary,
            self_ranges,
        });
    }
    Ok(data)
}
