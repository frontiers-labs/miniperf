//! Scheduler, block-IO and TCP tracepoints, through `bpftrace`.
//!
//! Deliberately has no privilege helper: callers either already have BPF access
//! or receive a degraded status.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use super::{Availability, SessionContext, Source, SourceDecl};
use crate::{Record, SourceStatus};

/// The metric table `bpftrace`'s `END` block fills in.
const GROUP: &str = "bpf";

/// Optional `bpftrace` tracepoint collector for the target's process tree.
#[derive(Default)]
pub struct BpfSource {
    child: Option<std::process::Child>,
    output: Option<PathBuf>,
    status: Option<SourceStatus>,
}

impl BpfSource {
    fn stop_child(&mut self) -> std::io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        child.wait()?;
        Ok(())
    }

    fn remove_output(&mut self) {
        if let Some(output) = self.output.take() {
            let _ = fs::remove_file(output);
        }
    }
}

impl Drop for BpfSource {
    fn drop(&mut self) {
        let _ = self.stop_child();
        self.remove_output();
    }
}

impl Source for BpfSource {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn declare(&self) -> SourceDecl {
        SourceDecl { name: "bpf" }
    }

    fn probe(&self, _directory: &Path) -> Availability {
        if !Path::new("/sys/kernel/btf/vmlinux").is_file() {
            return Availability::Unavailable {
                reason: "kernel BTF is unavailable".to_string(),
            };
        }
        Availability::Available
    }

    fn start(&mut self, context: &SessionContext) -> anyhow::Result<()> {
        anyhow::ensure!(self.child.is_none(), "BPF collection is already running");
        // Kept out of `probe`: the pass resolver records a probe failure as a
        // plain "unavailable", and losing the difference between "this kernel
        // forbids it" and "this host does not have it" loses the fix.
        if unsafe { libc::geteuid() } != 0 && unprivileged_bpf_disabled() {
            self.status = Some(unavailable(
                "unprivileged BPF is disabled; run with suitable BPF/perf capabilities",
            ));
            return Ok(());
        }
        let output = context.directory.join("snapshot-bpf.txt");
        let child = std::process::Command::new("bpftrace")
            .args(["-q", "-B", "line", "-o"])
            .arg(&output)
            .args(["-e", program(context.root_pid()).as_str()])
            .stderr(std::process::Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                self.status = Some(unavailable(&format!("could not start bpftrace: {error}")));
                return Ok(());
            }
        };
        self.child = Some(child);
        self.output = Some(output.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if fs::read_to_string(&output).is_ok_and(|value| value.contains("MPERF_READY")) {
                self.status = Some(SourceStatus::new(
                    "bpf",
                    "available",
                    "bpftrace/tracepoints",
                    "process_tree",
                    "scheduler, block, and TCP tracepoints",
                ));
                return Ok(());
            }
            if self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                self.child = None;
                self.remove_output();
                self.status = Some(unavailable(
                    "BPF program failed to load; inspect kernel capabilities and tracepoint availability",
                ));
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.stop_child();
        self.remove_output();
        self.status = Some(unavailable("timed out while loading BPF tracepoints"));
        Ok(())
    }

    fn stop(&mut self, context: &SessionContext) -> Vec<SourceStatus> {
        let Some(mut status) = self.status.take() else {
            return Vec::new();
        };
        if self.child.is_none() {
            return vec![status];
        }
        if self.stop_child().is_err() {
            status.status = "error".to_string();
            status.quality = "unavailable".to_string();
            status.message = "failed to stop BPF collector cleanly".to_string();
        }
        match self.output.as_ref().map(fs::read_to_string) {
            Some(Ok(text)) => {
                for (name, value) in metrics(&text) {
                    context.sink.record(Record::Metric {
                        group: GROUP,
                        name,
                        value,
                    });
                }
            }
            _ => {
                status.status = "error".to_string();
                status.message = "failed to read BPF metrics".to_string();
            }
        }
        self.remove_output();
        vec![status]
    }
}

fn unprivileged_bpf_disabled() -> bool {
    fs::read_to_string("/proc/sys/kernel/unprivileged_bpf_disabled")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0)
        != 0
}

fn unavailable(message: &str) -> SourceStatus {
    SourceStatus::new(
        "bpf",
        if message.contains("disabled") {
            "permission_denied"
        } else {
            "unavailable"
        },
        "bpftrace/tracepoints",
        "unavailable",
        message,
    )
}

/// The `MPERF <metric> <value>` lines `bpftrace` prints from its `END` block.
fn metrics(text: &str) -> Vec<(String, f64)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("MPERF")).then_some(())?;
            let name = fields.next()?.to_string();
            Some((name, fields.next()?.parse().ok()?))
        })
        .collect()
}

fn program(root_pid: u32) -> String {
    format!(
        r#"
BEGIN {{ @tracked[{root_pid}] = 1; printf("MPERF_READY\n"); }}
tracepoint:sched:sched_process_fork /@tracked[args->parent_pid]/ {{ @tracked[args->child_pid] = 1; }}
tracepoint:sched:sched_wakeup /@tracked[args->pid]/ {{ @wakeup[args->pid] = nsecs; }}
tracepoint:sched:sched_wakeup_new /@tracked[args->pid]/ {{ @wakeup[args->pid] = nsecs; }}
tracepoint:sched:sched_switch /@tracked[args->next_pid] && @wakeup[args->next_pid]/ {{ @runq_ns = sum(nsecs - @wakeup[args->next_pid]); @runq_count = count(); delete(@wakeup[args->next_pid]); }}
tracepoint:block:block_rq_issue /@tracked[pid]/ {{ @request[args->dev, args->sector] = nsecs; @block_bytes = sum(args->bytes); }}
tracepoint:block:block_rq_complete /@request[args->dev, args->sector]/ {{ @block_latency_ns = sum(nsecs - @request[args->dev, args->sector]); @block_count = count(); delete(@request[args->dev, args->sector]); }}
tracepoint:tcp:tcp_retransmit_skb /@tracked[pid]/ {{ @tcp_retransmits = count(); }}
END {{ printf("MPERF runq_ns %llu\n", @runq_ns); printf("MPERF runq_count %llu\n", @runq_count); printf("MPERF block_bytes %llu\n", @block_bytes); printf("MPERF block_latency_ns %llu\n", @block_latency_ns); printf("MPERF block_count %llu\n", @block_count); printf("MPERF tcp_retransmits %llu\n", @tcp_retransmits); }}
"#
    )
}
