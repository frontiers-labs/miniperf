use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub(super) struct ControlFlowGraph {
    entries: BTreeSet<u64>,
    successors: BTreeMap<u64, BTreeSet<u64>>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct NaturalLoop {
    pub header: u64,
    pub latches: BTreeSet<u64>,
    pub blocks: BTreeSet<u64>,
    pub parent: Option<usize>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct LoopAnalysis {
    pub natural_loops: Vec<NaturalLoop>,
    pub irreducible_cycles: Vec<BTreeSet<u64>>,
}

impl ControlFlowGraph {
    pub fn add_entry(&mut self, block: u64) {
        self.entries.insert(block);
        self.successors.entry(block).or_default();
    }

    pub fn add_edge(&mut self, from: u64, to: u64) {
        self.successors.entry(from).or_default().insert(to);
        self.successors.entry(to).or_default();
    }

    pub fn analyze(&self) -> LoopAnalysis {
        let nodes = self.reachable_nodes();
        if nodes.is_empty() {
            return LoopAnalysis::default();
        }
        let predecessors = predecessors(&nodes, &self.successors);
        let dominators = dominators(&nodes, &self.entries, &predecessors);
        let mut by_header = BTreeMap::<u64, NaturalLoop>::new();

        for (&tail, successors) in &self.successors {
            if !nodes.contains(&tail) {
                continue;
            }
            for &header in successors {
                if !dominators
                    .get(&tail)
                    .is_some_and(|dominated_by| dominated_by.contains(&header))
                {
                    continue;
                }
                let blocks = natural_loop_blocks(header, tail, &predecessors);
                let loop_info = by_header.entry(header).or_insert_with(|| NaturalLoop {
                    header,
                    latches: BTreeSet::new(),
                    blocks: BTreeSet::new(),
                    parent: None,
                });
                loop_info.latches.insert(tail);
                loop_info.blocks.extend(blocks);
            }
        }

        let mut natural_loops = by_header.into_values().collect::<Vec<_>>();
        natural_loops.sort_by_key(|loop_info| (loop_info.blocks.len(), loop_info.header));
        for child in 0..natural_loops.len() {
            natural_loops[child].parent = (0..natural_loops.len())
                .filter(|&candidate| {
                    candidate != child
                        && natural_loops[candidate]
                            .blocks
                            .is_superset(&natural_loops[child].blocks)
                        && natural_loops[candidate].blocks != natural_loops[child].blocks
                })
                .min_by_key(|&candidate| natural_loops[candidate].blocks.len());
        }

        let irreducible_cycles = strongly_connected_components(&nodes, &self.successors)
            .into_iter()
            .filter(|component| is_cycle(component, &self.successors))
            .filter(|component| {
                !component.iter().any(|header| {
                    dominators.contains_key(header)
                        && component.iter().all(|block| {
                            dominators
                                .get(block)
                                .is_some_and(|dominated_by| dominated_by.contains(header))
                        })
                })
            })
            .collect();

        LoopAnalysis {
            natural_loops,
            irreducible_cycles,
        }
    }

    fn reachable_nodes(&self) -> BTreeSet<u64> {
        let mut reachable = BTreeSet::new();
        let mut pending = self.entries.iter().copied().collect::<Vec<_>>();
        while let Some(block) = pending.pop() {
            if !reachable.insert(block) {
                continue;
            }
            pending.extend(self.successors.get(&block).into_iter().flatten().copied());
        }
        reachable
    }
}

fn predecessors(
    nodes: &BTreeSet<u64>,
    successors: &BTreeMap<u64, BTreeSet<u64>>,
) -> BTreeMap<u64, BTreeSet<u64>> {
    let mut predecessors = nodes
        .iter()
        .map(|&node| (node, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for (&from, targets) in successors {
        for &to in targets {
            if nodes.contains(&from) && nodes.contains(&to) {
                predecessors.entry(to).or_default().insert(from);
            }
        }
    }
    predecessors
}

fn dominators(
    nodes: &BTreeSet<u64>,
    entries: &BTreeSet<u64>,
    predecessors: &BTreeMap<u64, BTreeSet<u64>>,
) -> BTreeMap<u64, BTreeSet<u64>> {
    let roots = nodes
        .iter()
        .filter(|node| {
            entries.contains(node) || predecessors.get(node).is_none_or(BTreeSet::is_empty)
        })
        .copied()
        .collect::<BTreeSet<_>>();
    let mut result = nodes
        .iter()
        .map(|&node| {
            let initial = if roots.contains(&node) {
                BTreeSet::from([node])
            } else {
                nodes.clone()
            };
            (node, initial)
        })
        .collect::<BTreeMap<_, _>>();

    loop {
        let mut changed = false;
        for &node in nodes {
            if roots.contains(&node) {
                continue;
            }
            let mut incoming = predecessors
                .get(&node)
                .into_iter()
                .flatten()
                .filter_map(|predecessor| result.get(predecessor).cloned());
            let mut next = incoming.next().unwrap_or_default();
            for dominated_by in incoming {
                next = next.intersection(&dominated_by).copied().collect();
            }
            next.insert(node);
            if result.get(&node) != Some(&next) {
                result.insert(node, next);
                changed = true;
            }
        }
        if !changed {
            return result;
        }
    }
}

fn natural_loop_blocks(
    header: u64,
    latch: u64,
    predecessors: &BTreeMap<u64, BTreeSet<u64>>,
) -> BTreeSet<u64> {
    let mut blocks = BTreeSet::from([header, latch]);
    let mut pending = vec![latch];
    while let Some(block) = pending.pop() {
        if block == header {
            continue;
        }
        for predecessor in predecessors.get(&block).into_iter().flatten() {
            if blocks.insert(*predecessor) {
                pending.push(*predecessor);
            }
        }
    }
    blocks
}

fn strongly_connected_components(
    nodes: &BTreeSet<u64>,
    successors: &BTreeMap<u64, BTreeSet<u64>>,
) -> Vec<BTreeSet<u64>> {
    fn visit(
        node: u64,
        successors: &BTreeMap<u64, BTreeSet<u64>>,
        visited: &mut BTreeSet<u64>,
        order: &mut Vec<u64>,
    ) {
        if !visited.insert(node) {
            return;
        }
        for &target in successors.get(&node).into_iter().flatten() {
            visit(target, successors, visited, order);
        }
        order.push(node);
    }

    let mut order = Vec::new();
    let mut visited = BTreeSet::new();
    for &node in nodes {
        visit(node, successors, &mut visited, &mut order);
    }
    let predecessors = predecessors(nodes, successors);
    visited.clear();
    let mut components = Vec::new();
    while let Some(node) = order.pop() {
        if visited.contains(&node) {
            continue;
        }
        let mut component = BTreeSet::new();
        let mut pending = vec![node];
        while let Some(block) = pending.pop() {
            if !visited.insert(block) {
                continue;
            }
            component.insert(block);
            pending.extend(predecessors.get(&block).into_iter().flatten().copied());
        }
        components.push(component);
    }
    components
}

fn is_cycle(component: &BTreeSet<u64>, successors: &BTreeMap<u64, BTreeSet<u64>>) -> bool {
    component.len() > 1
        || component.iter().any(|node| {
            successors
                .get(node)
                .is_some_and(|targets| targets.contains(node))
        })
}
