use shmem::proc_channel::{Receiver, Sender};

/// Live collector statistics published to a local miniperf over the optional
/// shared-memory control channel. Remote ranks run file-only.
#[derive(Clone, Copy, Debug)]
pub struct CollectorStats {
    pub pid: u32,
    pub events: u64,
    pub dropped: u64,
    pub timestamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    Pause = 1,
    Resume = 2,
    Flush = 3,
}

impl CollectorStats {
    fn encode(self) -> [u8; 28] {
        let mut bytes = [0; 28];
        bytes[..4].copy_from_slice(&self.pid.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.events.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.dropped.to_le_bytes());
        bytes[20..].copy_from_slice(&self.timestamp.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 28] = bytes.try_into().ok()?;
        Some(Self {
            pid: u32::from_le_bytes(bytes[..4].try_into().ok()?),
            events: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
            dropped: u64::from_le_bytes(bytes[12..20].try_into().ok()?),
            timestamp: u64::from_le_bytes(bytes[20..].try_into().ok()?),
        })
    }
}

impl ControlCommand {
    fn encode(self) -> [u8; 4] {
        (self as u32).to_le_bytes()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        match u32::from_le_bytes(bytes.try_into().ok()?) {
            1 => Some(Self::Pause),
            2 => Some(Self::Resume),
            3 => Some(Self::Flush),
            _ => None,
        }
    }
}

const STATS_CHANNEL_BYTES: usize = 64 * 1024;
const COMMAND_CHANNEL_BYTES: usize = 4 * 1024;

/// Collector side of the control plane; present only when
/// `MPERF_CONTROL_SHMEM` names a channel prefix.
pub struct ControlPlane {
    stats: Sender,
    commands: Receiver,
}

impl ControlPlane {
    pub fn create(pid: u32) -> Option<ControlPlane> {
        let prefix = std::env::var("MPERF_CONTROL_SHMEM").ok()?;
        let stats = Sender::new(&format!("{prefix}-{pid}-stats"), STATS_CHANNEL_BYTES).ok()?;
        let commands = Receiver::new(&format!("{prefix}-{pid}-cmd"), COMMAND_CHANNEL_BYTES).ok()?;
        Some(ControlPlane { stats, commands })
    }

    pub fn publish(&self, stats: CollectorStats) {
        let _ = self.stats.send_sync(&stats.encode());
    }

    pub fn poll_command(&mut self) -> Option<ControlCommand> {
        while let Some(bytes) = self.commands.try_recv() {
            if let Some(command) = ControlCommand::decode(&bytes) {
                return Some(command);
            }
        }
        None
    }

    pub fn close(&self) {
        let _ = self.stats.close();
    }
}

/// miniperf side: attach to a collector's control channels.
pub struct ControlClient {
    stats: Receiver,
    commands: Sender,
}

impl ControlClient {
    /// Attach the sole control client to a live collector.
    ///
    /// # Safety
    /// No other client may attach to these initialized channels. No process may
    /// alter their mappings or metadata outside the channel protocol.
    pub unsafe fn attach(prefix: &str, pid: u32) -> std::io::Result<ControlClient> {
        Ok(ControlClient {
            stats: unsafe {
                Receiver::attach(&format!("{prefix}-{pid}-stats"), STATS_CHANNEL_BYTES)?
            },
            commands: unsafe {
                Sender::attach(&format!("{prefix}-{pid}-cmd"), COMMAND_CHANNEL_BYTES)?
            },
        })
    }

    pub fn latest_stats(&mut self) -> Option<CollectorStats> {
        let mut latest = None;
        while let Some(bytes) = self.stats.try_recv() {
            if let Some(stats) = CollectorStats::decode(&bytes) {
                latest = Some(stats);
            }
        }
        latest
    }

    pub fn send(&self, command: ControlCommand) {
        let _ = self.commands.send_sync(&command.encode());
    }
}
