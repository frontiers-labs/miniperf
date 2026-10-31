use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::{Record, SourceStatus};
use shmem::allocation::{Batch, Packet, CHANNEL_BYTES};
use shmem::proc_channel::Receiver;

/// Root-process libc allocation lifetimes. Attribution sampling never samples lifetimes.
pub struct AllocationSource {
    channel: String,
    stack_every: u64,
    receiver: Option<Receiver<Packet>>,
    error: Option<String>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Outcome>>,
}

#[derive(Default)]
struct Outcome {
    footer: bool,
    records: u64,
    produced: u64,
    producer_dropped: u64,
    channel_dropped: u64,
    packets_dropped: u64,
    paused: u64,
    invalid: bool,
}

impl AllocationSource {
    /// Create and own the channel before the child is created or released.
    pub fn new(stack_every: u64) -> Self {
        let channel = format!(
            "/mperf_alloc_{}_{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let result = Receiver::new(&channel, CHANNEL_BYTES);
        let (receiver, error) = match result {
            Ok(receiver) => (Some(receiver), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            channel,
            stack_every,
            receiver,
            error,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl Source for AllocationSource {
    fn declare(&self) -> SourceDecl {
        SourceDecl {
            name: "allocations",
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn probe(&self, _directory: &Path) -> Availability {
        let reason = if !cfg!(all(target_os = "linux", target_env = "gnu")) {
            Some("libc allocation preload is currently supported on Linux/glibc".to_string())
        } else if let Some(error) = &self.error {
            Some(error.clone())
        } else if libc_library_path().is_none()
            || super::internal_events::collector_library_path().is_none()
        {
            Some("released libc shim or collector library is missing".to_string())
        } else {
            None
        };
        reason.map_or(Availability::Available, |reason| {
            Availability::Unavailable { reason }
        })
    }
    fn child_environment(&self, directory: &Path) -> Vec<(String, String)> {
        let mut env = super::InternalEventsSource {
            roofline_instrumented: false,
        }
        .child_environment(directory);
        env.push(("MPERF_ALLOCATION_CHANNEL".into(), self.channel.clone()));
        env.push((
            "MPERF_ALLOCATION_STACK_EVERY".into(),
            self.stack_every.to_string(),
        ));
        if let Some(library) = libc_library_path() {
            let preload = std::env::var("LD_PRELOAD")
                .ok()
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || library.display().to_string(),
                    |current| format!("{}:{current}", library.display()),
                );
            env.push(("LD_PRELOAD".into(), preload));
        }
        env
    }
    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()> {
        if !context.launched() {
            return Ok(());
        }
        let receiver = self
            .receiver
            .take()
            .ok_or_else(|| anyhow::anyhow!("allocation channel is missing"))?;
        let stop = self.stop.clone();
        let sink = context.sink.clone();
        let root = context.root_pid() as u64;
        self.worker = Some(std::thread::spawn(move || {
            let mut result = Outcome::default();
            let mut expected = 1;
            let mut missing_packets = 0;
            loop {
                while let Some(packet) = receiver.try_recv() {
                    let Some(batch) = Batch::decode(&packet) else {
                        result.invalid = true;
                        continue;
                    };
                    if batch.sequence < expected || result.footer {
                        result.invalid = true;
                    }
                    missing_packets += batch.sequence.saturating_sub(expected);
                    expected = batch.sequence + 1;
                    result.produced = batch.produced;
                    result.producer_dropped = batch.producer_dropped;
                    result.channel_dropped = batch.channel_dropped_operations;
                    result.paused = batch.paused;
                    result.footer = batch.footer;
                    for record in batch.records {
                        if record.pid != root {
                            result.invalid = true;
                            continue;
                        }
                        result.records += 1;
                        sink.record(Record::Allocation(domain_record(record)));
                    }
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            result.packets_dropped = receiver.dropped_count() as u64;
            result.invalid |= missing_packets > result.packets_dropped;
            result
        }));
        Ok(())
    }
    fn stop(&mut self, context: &SessionContext) -> Vec<SourceStatus> {
        if !context.launched() {
            return vec![SourceStatus::new(
                "allocations",
                "unavailable",
                "libc_preload",
                "missing",
                "Attach cannot inject allocation instrumentation into a running target; launch with --allocations",
            )];
        }
        self.stop.store(true, Ordering::Release);
        let result = self
            .worker
            .take()
            .and_then(|worker| worker.join().ok())
            .unwrap_or_default();
        let complete = result.footer
            && !result.invalid
            && result.producer_dropped == 0
            && result.channel_dropped == 0
            && result.packets_dropped == 0
            && result.paused == 0
            && result.produced == result.records;
        context
            .sink
            .record(Record::AllocationCoverage(crate::AllocationCoverage {
                pid: context.root_pid() as u64,
                complete,
                footer: result.footer,
                produced: result.produced,
                observed_operations: result.records,
                producer_dropped: result.producer_dropped,
                channel_dropped_operations: result.channel_dropped,
                channel_dropped_packets: result.packets_dropped,
                paused_operations: result.paused,
                invalid_protocol: result.invalid,
                stack_every: self.stack_every,
            }));
        let message = format!(
            "root-process launch scope; libc-interposable allocator calls only; fork children, static/custom allocators excluded; footer={}; observed_operations={}; produced={}; producer_dropped={}; channel_dropped_operations={}; channel_dropped_packets={}; paused_operations={}; invalid_protocol={}; stack_every={}; requested bytes, not RSS; realloc intervals have uncertain internal peaks",
            result.footer,
            result.records,
            result.produced,
            result.producer_dropped,
            result.channel_dropped,
            result.packets_dropped,
            result.paused,
            result.invalid,
            self.stack_every
        );
        vec![SourceStatus::new(
            "allocations",
            if complete { "available" } else { "degraded" },
            "libc_preload",
            if complete {
                "observed_root_lifetimes"
            } else {
                "incomplete"
            },
            &message,
        )]
    }
}

fn libc_library_path() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.to_owned();
    [
        path.join("libmperf_libc.so"),
        path.join("../lib/miniperf/libmperf_libc.so"),
        path.join("../lib/libmperf_libc.so"),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

impl Drop for AllocationSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn domain_record(record: shmem::allocation::Allocation) -> crate::Allocation {
    use crate::{AllocationAttribution as Attribution, AllocationOperation as Operation};
    let frames = record.frames[..record.frame_count].to_vec();
    let attribution = match record.stack_status {
        0 => Attribution::NotSelected,
        1 => Attribution::Captured(frames),
        2 => Attribution::Unavailable,
        3 => Attribution::Truncated(frames),
        _ => Attribution::UnwindError(frames),
    };
    let operation = match record.operation {
        shmem::allocation::Operation::Allocated => Operation::Allocated,
        shmem::allocation::Operation::Freed => Operation::Freed,
        shmem::allocation::Operation::ReallocBegin => Operation::ReallocBegin,
        shmem::allocation::Operation::ReallocEnd => Operation::ReallocEnd,
    };
    crate::Allocation {
        pid: record.pid as u32,
        process_epoch: record.process_epoch,
        sequence: record.sequence,
        timestamp_ns: record.timestamp_ns,
        tid: record.tid as u32,
        operation,
        operation_id: record.operation_id,
        old_address: record.old_address,
        address: record.address,
        requested_bytes: record.requested_bytes,
        retires_old: record.retires_old,
        attribution,
    }
}
