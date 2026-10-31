use shmem::proc_channel::{Receiver, Sender};

/// Live collector statistics published to a local miniperf over the optional
/// shared-memory control channel. Remote ranks run file-only.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CollectorStats {
    pub pid: u32,
    pub events: u64,
    pub dropped: u64,
    pub timestamp: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    Pause = 1,
    Resume = 2,
    Flush = 3,
}

const STATS_CHANNEL_BYTES: usize = 64 * 1024;
const COMMAND_CHANNEL_BYTES: usize = 4 * 1024;

/// Collector side of the control plane; present only when
/// `MPERF_CONTROL_SHMEM` names a channel prefix.
pub struct ControlPlane {
    stats: Sender<CollectorStats>,
    commands: Receiver<ControlCommand>,
}

impl ControlPlane {
    pub fn create(pid: u32) -> Option<ControlPlane> {
        let prefix = std::env::var("MPERF_CONTROL_SHMEM").ok()?;
        let stats = Sender::new(&format!("{prefix}-{pid}-stats"), STATS_CHANNEL_BYTES).ok()?;
        let commands = Receiver::new(&format!("{prefix}-{pid}-cmd"), COMMAND_CHANNEL_BYTES).ok()?;
        Some(ControlPlane { stats, commands })
    }

    pub fn publish(&self, stats: CollectorStats) {
        let _ = self.stats.send_sync(&stats);
    }

    pub fn poll_command(&self) -> Option<ControlCommand> {
        if self.commands.empty() {
            return None;
        }
        self.commands.recv_sync()
    }

    pub fn close(&self) {
        let _ = self.stats.close();
    }
}

/// miniperf side: attach to a collector's control channels.
pub struct ControlClient {
    stats: Receiver<CollectorStats>,
    commands: Sender<ControlCommand>,
}

impl ControlClient {
    pub fn attach(prefix: &str, pid: u32) -> std::io::Result<ControlClient> {
        Ok(ControlClient {
            stats: Receiver::attach(&format!("{prefix}-{pid}-stats"), STATS_CHANNEL_BYTES)?,
            commands: Sender::attach(&format!("{prefix}-{pid}-cmd"), COMMAND_CHANNEL_BYTES)?,
        })
    }

    pub fn latest_stats(&self) -> Option<CollectorStats> {
        let mut latest = None;
        while !self.stats.empty() {
            if let Some(stats) = self.stats.recv_sync() {
                latest = Some(stats);
            } else {
                break;
            }
        }
        latest
    }

    pub fn send(&self, command: ControlCommand) {
        let _ = self.commands.send_sync(&command);
    }
}
