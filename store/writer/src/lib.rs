//! Parquet session-directory writing and recovery for miniperf.

mod clock;
mod hash;
mod interner;
mod recover;
mod tables;
mod writer;

pub use hash::{stack_hash, string_hash, xxh3};
pub use interner::StringInterner;
pub use recover::{RecoveryReport, recover_session};
pub use tables::{
    ClockAnchorRows, ClockSyncRows, DeviceClockRows, EventKind, EventMetaRows, EventRows,
    MemSampleRawRows, ModuleRows, PayloadRows, SampleRawRows, SampleRows, StackRows, StringRows,
    fit_device_clock,
};
pub use writer::{SegmentWriter, write_table};

pub use arrow;
pub use parquet;

/// On-disk results format written by this build. Version 3 is the all-Parquet
/// session directory; the XXH3-64 identity hash is part of this contract.
pub const CURRENT_FORMAT_VERSION: u32 = 3;
