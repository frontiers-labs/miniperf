//! Dynamic CFG accounting shared by the instrumentation backends.

use crate::classify::{BlockCost, FlowKind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DynamicBlockCounts {
    pub end_vaddr: u64,
    pub executions: u64,
    pub scalar_int: u64,
    pub scalar_float: u64,
    pub scalar_double: u64,
    pub vector_int: u64,
    pub vector_float: u64,
    pub vector_double: u64,
    /// Modeled DRAM traffic attributed to this block. Cache-resident loops
    /// legitimately have zero here, which is why arithmetic intensity must not
    /// be derived from these alone.
    pub bytes_load: u64,
    pub bytes_store: u64,
    /// Architectural bytes the block's memory operands moved, independent of
    /// the cache model. Always non-zero for a block that touches memory, so
    /// this is what gives every loop a finite arithmetic intensity.
    pub arch_bytes_load: u64,
    pub arch_bytes_store: u64,
    pub unclassified: u64,
    /// Executions per thread, for blocks recorded with a thread identity.
    /// Counted blocks (a shared counter with no thread) leave this empty.
    pub threads: Vec<(u32, u64)>,
}

fn add_thread_executions(block: &mut DynamicBlockCounts, thread: u32, count: u64) {
    match block
        .threads
        .iter_mut()
        .find(|(candidate, _)| *candidate == thread)
    {
        Some((_, executions)) => *executions = executions.saturating_add(count),
        None => block.threads.push((thread, count)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorClass {
    Integer,
    Float,
    Double,
}

pub struct DynamicCfg {
    last_blocks: Vec<Option<(u64, FlowKind)>>,
    call_stacks: Vec<Vec<u64>>,
    pub entries: BTreeSet<u64>,
    pub edges: BTreeMap<(u64, u64), u64>,
    pub blocks: BTreeMap<u64, DynamicBlockCounts>,
}

impl DynamicCfg {
    pub const fn new() -> Self {
        Self {
            last_blocks: Vec::new(),
            call_stacks: Vec::new(),
            entries: BTreeSet::new(),
            edges: BTreeMap::new(),
            blocks: BTreeMap::new(),
        }
    }

    /// Records one execution of a block: control-flow edge bookkeeping plus
    /// accumulation of the block's static cost.
    pub fn record_block(&mut self, vcpu: usize, cost: &BlockCost) {
        if self.last_blocks.len() <= vcpu {
            self.last_blocks.resize(vcpu + 1, None);
        }
        if self.call_stacks.len() <= vcpu {
            self.call_stacks.resize_with(vcpu + 1, Vec::new);
        }
        if let Some((previous, flow)) = self.last_blocks[vcpu] {
            match flow {
                FlowKind::Normal => {
                    *self.edges.entry((previous, cost.vaddr)).or_default() += 1;
                }
                FlowKind::Call => {
                    self.entries.insert(cost.vaddr);
                    self.call_stacks[vcpu].push(previous);
                }
                FlowKind::Return => {
                    if let Some(caller) = self.call_stacks[vcpu].pop() {
                        // Summarize the call as an intraprocedural edge from the
                        // call block to its observed continuation. This preserves
                        // caller loops without adding call/return edges that can
                        // create false interprocedural cycles.
                        *self.edges.entry((caller, cost.vaddr)).or_default() += 1;
                    } else {
                        self.entries.insert(cost.vaddr);
                    }
                }
            }
        } else {
            self.entries.insert(cost.vaddr);
        }
        self.last_blocks[vcpu] = Some((cost.vaddr, cost.flow));
        let block = self.blocks.entry(cost.vaddr).or_default();
        if block.end_vaddr == 0 {
            block.end_vaddr = cost.end_vaddr;
        } else if block.end_vaddr != cost.end_vaddr {
            // Self-modifying code or context-dependent translation at the same
            // virtual address cannot be represented as one stable source range.
            block.end_vaddr = block.end_vaddr.max(cost.end_vaddr);
        }
        block.executions = block.executions.saturating_add(1);
        add_thread_executions(block, vcpu as u32, 1);
        block.scalar_int = block.scalar_int.saturating_add(cost.scalar_int);
        block.scalar_float = block.scalar_float.saturating_add(cost.scalar_float);
        block.scalar_double = block.scalar_double.saturating_add(cost.scalar_double);
        block.vector_int = block.vector_int.saturating_add(cost.vector_int);
        block.vector_float = block.vector_float.saturating_add(cost.vector_float);
        block.vector_double = block.vector_double.saturating_add(cost.vector_double);
    }

    /// Records `count` additional back-to-back executions of a block that just
    /// executed with `FlowKind::Normal` flow (a self loop). Equivalent to
    /// calling `record_block` `count` more times on the same vcpu, but with a
    /// single edge/block update. Callers must ensure the block is the most
    /// recent one recorded for its vcpu.
    pub fn record_repeats(&mut self, vcpu: usize, cost: &BlockCost, count: u64) {
        if count == 0 {
            return;
        }
        *self.edges.entry((cost.vaddr, cost.vaddr)).or_default() += count;
        let block = self.blocks.entry(cost.vaddr).or_default();
        block.executions = block.executions.saturating_add(count);
        add_thread_executions(block, vcpu as u32, count);
        block.scalar_int = block
            .scalar_int
            .saturating_add(cost.scalar_int.saturating_mul(count));
        block.scalar_float = block
            .scalar_float
            .saturating_add(cost.scalar_float.saturating_mul(count));
        block.scalar_double = block
            .scalar_double
            .saturating_add(cost.scalar_double.saturating_mul(count));
        block.vector_int = block
            .vector_int
            .saturating_add(cost.vector_int.saturating_mul(count));
        block.vector_float = block
            .vector_float
            .saturating_add(cost.vector_float.saturating_mul(count));
        block.vector_double = block
            .vector_double
            .saturating_add(cost.vector_double.saturating_mul(count));
    }

    /// Adds executions collected by a direct counter plus static successors.
    /// These blocks bypass the ordered event stream to keep their hot path to
    /// one counter update.
    pub fn record_counted_block(&mut self, cost: &BlockCost, count: u64, successors: &[u64]) {
        if count == 0 {
            return;
        }
        let block = self.blocks.entry(cost.vaddr).or_default();
        block.end_vaddr = block.end_vaddr.max(cost.end_vaddr);
        block.executions = block.executions.saturating_add(count);
        block.scalar_int = block
            .scalar_int
            .saturating_add(cost.scalar_int.saturating_mul(count));
        block.scalar_float = block
            .scalar_float
            .saturating_add(cost.scalar_float.saturating_mul(count));
        block.scalar_double = block
            .scalar_double
            .saturating_add(cost.scalar_double.saturating_mul(count));
        block.vector_int = block
            .vector_int
            .saturating_add(cost.vector_int.saturating_mul(count));
        block.vector_float = block
            .vector_float
            .saturating_add(cost.vector_float.saturating_mul(count));
        block.vector_double = block
            .vector_double
            .saturating_add(cost.vector_double.saturating_mul(count));
        for &successor in successors {
            let edge_count = if successor == cost.vaddr {
                count.saturating_sub(1)
            } else {
                count
            };
            if edge_count != 0 {
                *self.edges.entry((cost.vaddr, successor)).or_default() += edge_count;
            }
        }
    }

    pub fn attribute_unclassified(&mut self, block_address: u64) {
        let block = self.blocks.entry(block_address).or_default();
        block.unclassified = block.unclassified.saturating_add(1);
    }

    /// Attributes one block's memory accesses: the architectural bytes its
    /// operands moved plus the modeled DRAM traffic they caused. Both are
    /// accumulated under a single lookup because this is the hottest path in
    /// the instrumentation backends.
    pub fn attribute_memory(
        &mut self,
        block_address: u64,
        arch_bytes_load: u64,
        arch_bytes_store: u64,
        dram_bytes_load: u64,
        dram_bytes_store: u64,
    ) {
        let block = self.blocks.entry(block_address).or_default();
        block.arch_bytes_load = block.arch_bytes_load.saturating_add(arch_bytes_load);
        block.arch_bytes_store = block.arch_bytes_store.saturating_add(arch_bytes_store);
        block.bytes_load = block.bytes_load.saturating_add(dram_bytes_load);
        block.bytes_store = block.bytes_store.saturating_add(dram_bytes_store);
    }

    /// Attributes dynamically-counted vector operations (RVV) to a block.
    pub fn attribute_vector(&mut self, block_address: u64, class: VectorClass, operations: u64) {
        let block = self.blocks.entry(block_address).or_default();
        match class {
            VectorClass::Integer => {
                block.vector_int = block.vector_int.saturating_add(operations);
            }
            VectorClass::Float => {
                block.vector_float = block.vector_float.saturating_add(operations);
            }
            VectorClass::Double => {
                block.vector_double = block.vector_double.saturating_add(operations);
            }
        }
    }
}

impl Default for DynamicCfg {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_blocks_preserve_costs_and_static_successors() {
        let mut cfg = DynamicCfg::new();
        let cost = BlockCost {
            vaddr: 0x100,
            end_vaddr: 0x120,
            flow: FlowKind::Normal,
            scalar_int: 2,
            vector_double: 8,
            instructions: 5,
            ..BlockCost::default()
        };
        cfg.record_counted_block(&cost, 10, &[0x100, 0x120]);

        let block = &cfg.blocks[&0x100];
        assert_eq!(block.executions, 10);
        assert_eq!(block.scalar_int, 20);
        assert_eq!(block.vector_double, 80);
        assert_eq!(cfg.edges[&(0x100, 0x100)], 9);
        assert_eq!(cfg.edges[&(0x100, 0x120)], 10);
        assert!(block.threads.is_empty());
    }

    #[test]
    fn executions_are_kept_per_thread() {
        let mut cfg = DynamicCfg::new();
        let cost = BlockCost {
            vaddr: 0x100,
            end_vaddr: 0x120,
            flow: FlowKind::Normal,
            scalar_double: 4,
            instructions: 5,
            ..BlockCost::default()
        };
        cfg.record_block(0, &cost);
        cfg.record_block(1, &cost);
        cfg.record_repeats(1, &cost, 9);
        cfg.record_block(0, &cost);

        let block = &cfg.blocks[&0x100];
        assert_eq!(block.executions, 12);
        assert_eq!(block.scalar_double, 48);
        assert_eq!(block.threads, vec![(0, 2), (1, 10)]);
    }
}
