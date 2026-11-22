//! Exact working-set / reuse-distance / spatial-utilization analysis shared by
//! the QEMU plugin and the DynamoRIO client.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

pub const WORKING_SET_WINDOWS: [u64; 6] = [1_024, 4_096, 16_384, 65_536, 262_144, 1_048_576];

#[derive(Default)]
struct LineFootprint {
    bytes: [u64; 4],
    last_touch: u64,
}

impl LineFootprint {
    fn touch(&mut self, offset: u64, length: u64, line_size: u64) {
        let start = offset.min(line_size).min(256);
        let end = offset.saturating_add(length).min(line_size).min(256);
        if start >= end {
            return;
        }
        let first_word = (start / 64) as usize;
        let last_word = ((end - 1) / 64) as usize;
        for (word, bits) in self
            .bytes
            .iter_mut()
            .enumerate()
            .take(last_word + 1)
            .skip(first_word)
        {
            let word_start = (word as u64).saturating_mul(64);
            let first_bit = start.saturating_sub(word_start).min(64) as u32;
            let last_bit = end.saturating_sub(word_start).min(64) as u32;
            let below_last = if last_bit == 64 {
                u64::MAX
            } else {
                (1_u64 << last_bit) - 1
            };
            let below_first = if first_bit == 0 {
                0
            } else {
                (1_u64 << first_bit) - 1
            };
            *bits |= below_last & !below_first;
        }
    }

    fn count(&self) -> u64 {
        self.bytes.iter().map(|word| word.count_ones() as u64).sum()
    }
}

#[derive(Default)]
struct WindowAccumulator {
    width: u64,
    references: u64,
    window_start: u64,
    unique_lines: u64,
    samples: Vec<u64>,
}

impl WindowAccumulator {
    fn new(width: u64) -> Self {
        Self {
            width,
            ..Self::default()
        }
    }

    fn observe(&mut self, previous_touch: u64, reference: u64) {
        if self.references == 0 {
            self.window_start = reference;
        }
        self.references += 1;
        if previous_touch < self.window_start {
            self.unique_lines += 1;
        }
        if self.references == self.width {
            if self.samples.len() < 1_000_000 {
                self.samples.push(self.unique_lines);
            }
            self.references = 0;
            self.unique_lines = 0;
        }
    }
}

pub(crate) struct OrderNode {
    pub key: u64,
    pub priority: u64,
    pub size: u64,
    pub left: Option<Box<OrderNode>>,
    pub right: Option<Box<OrderNode>>,
}

pub(crate) fn node_size(node: &Option<Box<OrderNode>>) -> u64 {
    node.as_ref().map_or(0, |node| node.size)
}

fn refresh(node: &mut Box<OrderNode>) {
    node.size = 1 + node_size(&node.left) + node_size(&node.right);
}

fn split(
    root: Option<Box<OrderNode>>,
    key: u64,
) -> (Option<Box<OrderNode>>, Option<Box<OrderNode>>) {
    let Some(mut root) = root else {
        return (None, None);
    };
    if root.key < key {
        let (left, right) = split(root.right.take(), key);
        root.right = left;
        refresh(&mut root);
        (Some(root), right)
    } else {
        let (left, right) = split(root.left.take(), key);
        root.left = right;
        refresh(&mut root);
        (left, Some(root))
    }
}

fn merge(left: Option<Box<OrderNode>>, right: Option<Box<OrderNode>>) -> Option<Box<OrderNode>> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(mut left), Some(right)) if left.priority >= right.priority => {
            left.right = merge(left.right.take(), Some(right));
            refresh(&mut left);
            Some(left)
        }
        (Some(left), Some(mut right)) => {
            right.left = merge(Some(left), right.left.take());
            refresh(&mut right);
            Some(right)
        }
    }
}

pub(crate) fn insert(root: Option<Box<OrderNode>>, node: Box<OrderNode>) -> Option<Box<OrderNode>> {
    let (left, right) = split(root, node.key);
    merge(merge(left, Some(node)), right)
}

/// Removes `key` while returning its allocation for reuse at the new recency
/// position. A hot line used to allocate and free one tree node on every
/// access, which dominated address-heavy profiles.
fn remove_and_retain(
    root: Option<Box<OrderNode>>,
    key: u64,
) -> (Option<Box<OrderNode>>, Option<Box<OrderNode>>, u64) {
    let Some(mut root) = root else {
        return (None, None, 0);
    };
    if root.key == key {
        let newer = node_size(&root.right);
        let merged = merge(root.left.take(), root.right.take());
        root.size = 1;
        return (merged, Some(root), newer);
    }
    let (removed, newer);
    if key < root.key {
        let right_size = node_size(&root.right);
        let (left, found, below) = remove_and_retain(root.left.take(), key);
        root.left = left;
        removed = found;
        newer = below.saturating_add(1).saturating_add(right_size);
    } else {
        let (right, found, below) = remove_and_retain(root.right.take(), key);
        root.right = right;
        removed = found;
        newer = below;
    }
    refresh(&mut root);
    (Some(root), removed, newer)
}

#[cfg(test)]
fn count_greater(root: &Option<Box<OrderNode>>, key: u64) -> u64 {
    let Some(root) = root else { return 0 };
    if root.key > key {
        1 + node_size(&root.right) + count_greater(&root.left, key)
    } else {
        count_greater(&root.right, key)
    }
}

pub(crate) fn priority(key: u64) -> u64 {
    let mut value = key.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

pub struct MemoryAnalysis {
    line_size: u64,
    references: u64,
    load_bytes: u64,
    store_bytes: u64,
    cold_references: u64,
    footprints: HashMap<u64, LineFootprint>,
    recency: Option<Box<OrderNode>>,
    reuse_distance: [u64; 65],
    last_line_by_vcpu: Vec<Option<u64>>,
    strides: [u64; 129],
    windows: Vec<WindowAccumulator>,
}

impl MemoryAnalysis {
    pub fn new(line_size: u64) -> Self {
        Self {
            line_size,
            references: 0,
            load_bytes: 0,
            store_bytes: 0,
            cold_references: 0,
            footprints: HashMap::new(),
            recency: None,
            reuse_distance: [0; 65],
            last_line_by_vcpu: Vec::new(),
            strides: [0; 129],
            windows: WORKING_SET_WINDOWS
                .into_iter()
                .map(WindowAccumulator::new)
                .collect(),
        }
    }

    pub fn access(&mut self, vcpu: usize, address: u64, size: u64, store: bool) {
        if store {
            self.store_bytes = self.store_bytes.saturating_add(size);
        } else {
            self.load_bytes = self.load_bytes.saturating_add(size);
        }
        if size == 0 {
            return;
        }
        let first = address / self.line_size;
        let last = address.saturating_add(size - 1) / self.line_size;
        for line in first..=last {
            self.references = self.references.saturating_add(1);
            let line_start = line.saturating_mul(self.line_size);
            let start = address.max(line_start);
            let end = address
                .saturating_add(size)
                .min(line_start.saturating_add(self.line_size));
            let footprint = self.footprints.entry(line).or_default();
            footprint.touch(start - line_start, end - start, self.line_size);
            let previous = footprint.last_touch;
            footprint.last_touch = self.references;
            for window in &mut self.windows {
                window.observe(previous, self.references);
            }
            let mut node = None;
            if previous != 0 {
                let distance;
                (self.recency, node, distance) = remove_and_retain(self.recency.take(), previous);
                let bucket = if distance == 0 {
                    0
                } else {
                    64 - distance.leading_zeros()
                };
                self.reuse_distance[bucket as usize] += 1;
            } else {
                self.cold_references += 1;
            }
            let key = self.references;
            let mut node = node.unwrap_or_else(|| {
                Box::new(OrderNode {
                    key,
                    priority: priority(key),
                    size: 1,
                    left: None,
                    right: None,
                })
            });
            node.key = key;
            node.priority = priority(key);
            node.size = 1;
            node.left = None;
            node.right = None;
            self.recency = insert(self.recency.take(), node);

            if self.last_line_by_vcpu.len() <= vcpu {
                self.last_line_by_vcpu.resize(vcpu + 1, None);
            }
            if let Some(previous) = self.last_line_by_vcpu[vcpu] {
                let delta = line as i128 - previous as i128;
                let magnitude = delta.unsigned_abs() as u64;
                let log = if magnitude == 0 {
                    0
                } else {
                    64 - magnitude.leading_zeros() as i32
                };
                let bucket = if delta < 0 { -log } else { log };
                self.strides[(bucket + 64) as usize] += 1;
            }
            self.last_line_by_vcpu[vcpu] = Some(line);
        }
    }

    pub fn artifact(&self) -> MemoryArtifact {
        let distinct_bytes = self.footprints.values().map(LineFootprint::count).sum();
        let mut utilization = BTreeMap::new();
        for line in self.footprints.values() {
            let percent =
                (line.count().saturating_mul(100) / self.line_size.max(1)).min(100) as u32;
            let bucket = (percent / 10) * 10;
            *utilization.entry(bucket).or_default() += 1;
        }
        let working_set = self
            .windows
            .iter()
            .map(|window| {
                let mut samples = window.samples.clone();
                if window.references != 0 {
                    samples.push(window.unique_lines);
                }
                samples.sort_unstable();
                let mean = if samples.is_empty() {
                    0.0
                } else {
                    samples.iter().sum::<u64>() as f64 / samples.len() as f64
                };
                let p95 = samples
                    .get(samples.len().saturating_sub(1) * 95 / 100)
                    .copied()
                    .unwrap_or(0);
                WorkingSetArtifact {
                    window_references: window.width,
                    mean_lines: mean,
                    p95_lines: p95,
                    max_lines: samples.last().copied().unwrap_or(0),
                }
            })
            .collect();
        let reuse_distance = self
            .reuse_distance
            .iter()
            .enumerate()
            .filter(|(_, count)| **count != 0)
            .map(|(bucket, count)| (bucket as u32, *count))
            .collect();
        let strides = self
            .strides
            .iter()
            .enumerate()
            .filter(|(_, count)| **count != 0)
            .map(|(bucket, count)| (bucket as i32 - 64, *count))
            .collect();
        MemoryArtifact {
            format_version: 1,
            line_size: self.line_size,
            references: self.references,
            architectural_load_bytes: self.load_bytes,
            architectural_store_bytes: self.store_bytes,
            unique_lines: self.footprints.len() as u64,
            distinct_bytes,
            cold_references: self.cold_references,
            reuse_distance_log2: reuse_distance,
            spatial_utilization_percent: utilization,
            stride_lines_log2: strides,
            working_set,
        }
    }
}

#[derive(Serialize)]
pub struct WorkingSetArtifact {
    pub window_references: u64,
    pub mean_lines: f64,
    pub p95_lines: u64,
    pub max_lines: u64,
}

#[derive(Serialize)]
pub struct MemoryArtifact {
    pub format_version: u32,
    pub line_size: u64,
    pub references: u64,
    pub architectural_load_bytes: u64,
    pub architectural_store_bytes: u64,
    pub unique_lines: u64,
    pub distinct_bytes: u64,
    pub cold_references: u64,
    pub reuse_distance_log2: BTreeMap<u32, u64>,
    pub spatial_utilization_percent: BTreeMap<u32, u64>,
    pub stride_lines_log2: BTreeMap<i32, u64>,
    pub working_set: Vec<WorkingSetArtifact>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_analysis_counts_footprint_spatial_and_exact_stack_distance() {
        let mut analysis = MemoryAnalysis::new(64);
        analysis.access(0, 0, 8, false); // A
        analysis.access(0, 64, 8, false); // B
        analysis.access(0, 128, 8, false); // C
        analysis.access(0, 0, 8, false); // A, two newer distinct lines
        analysis.access(0, 0, 8, true); // A, immediate reuse
        let artifact = analysis.artifact();
        assert_eq!(artifact.references, 5);
        assert_eq!(artifact.unique_lines, 3);
        assert_eq!(artifact.distinct_bytes, 24);
        assert_eq!(artifact.cold_references, 3);
        assert_eq!(artifact.architectural_load_bytes, 32);
        assert_eq!(artifact.architectural_store_bytes, 8);
        assert_eq!(artifact.reuse_distance_log2.get(&2), Some(&1));
        assert_eq!(artifact.reuse_distance_log2.get(&0), Some(&1));
        assert_eq!(artifact.spatial_utilization_percent.get(&10), Some(&3));
    }

    #[test]
    fn order_statistic_tree_tracks_more_recent_unique_lines() {
        let mut root = None;
        for key in [10, 20, 30, 40] {
            root = insert(
                root,
                Box::new(OrderNode {
                    key,
                    priority: priority(key),
                    size: 1,
                    left: None,
                    right: None,
                }),
            );
        }
        assert_eq!(count_greater(&root, 20), 2);
        let (new_root, removed, newer) = remove_and_retain(root, 30);
        root = new_root;
        assert_eq!(removed.unwrap().key, 30);
        assert_eq!(newer, 1);
        assert_eq!(count_greater(&root, 20), 1);
        assert_eq!(node_size(&root), 3);
    }

    #[test]
    fn footprint_marks_ranges_without_a_per_byte_loop() {
        let mut footprint = LineFootprint::default();
        footprint.touch(60, 8, 128);
        footprint.touch(126, 2, 128);
        assert_eq!(footprint.count(), 10);
    }

    #[test]
    fn working_set_windows_remain_exact_without_per_window_hash_sets() {
        let mut analysis = MemoryAnalysis::new(64);
        for index in 0..512 {
            analysis.access(0, (index % 2) * 64, 8, false);
        }
        for line in 2..514 {
            analysis.access(0, line * 64, 8, false);
        }
        for _ in 0..1_024 {
            analysis.access(0, 0, 8, false);
        }

        let artifact = analysis.artifact();
        let first_window = &artifact.working_set[0];
        assert_eq!(first_window.window_references, 1_024);
        assert_eq!(first_window.mean_lines, 257.5);
        assert_eq!(first_window.max_lines, 514);
    }
}
